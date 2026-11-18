//! What the analysis knows from a crate's Rust code, read with `syn`: its custom filters, and the structs that derive `Template`, with the
//! template each names and its fields and methods.
//!
//! Only the syntax is read, so field types are as written and nothing is resolved. Each file is reread only when its modification time
//! changes, and a crate's merged facts are rebuilt only when one of its files does. Files are read from disk, so a change counts once it is
//! saved.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use quote::ToTokens;
use syn::spanned::Spanned;

use crate::filters::{CrateFilters, CustomFilter};
use crate::lines::{LineIndex, Position};
use crate::project::{MAX_WALKED_FILES, Visited};

/// Whether a member is a field or a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    /// A named field of the struct.
    Field,
    /// A method with a `self` receiver, from an inherent `impl` block.
    Method,
}

/// A field of a template struct, or a method of one with a `self` receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The field's or method's name.
    pub name: String,
    /// Whether it is a field or a method.
    pub kind: MemberKind,
    /// A field's type, or a method's signature, as written with whitespace collapsed.
    pub detail: String,
    /// Its `///` doc comment, one line per line, or empty.
    pub docs: String,
    /// The Rust file it is written in.
    pub file: Arc<Path>,
    /// The name's range in `file`.
    pub range: Range<usize>,
}

/// A struct that derives `Template` from a template file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateStruct {
    /// The struct's name.
    pub name: String,
    /// The template's name from `#[template(path = "...")]`.
    pub template: String,
    /// The range of that name, inside the quotes, in `file`.
    pub template_range: Range<usize>,
    /// The Rust file the struct is written in.
    pub file: Arc<Path>,
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
    /// The member called `name`, if the struct has one.
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|member| member.name == name)
    }
}

/// Everything read from one crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrateFacts {
    /// The crate's custom filters.
    pub filters: CrateFilters,
    /// Its structs that derive `Template` from a template file.
    pub structs: Vec<TemplateStruct>,
}

/// What one Rust file contributes to its crate's facts.
#[derive(Debug, Default)]
struct FileFacts {
    /// The filters it defines.
    filters: CrateFilters,
    /// The template structs it defines.
    structs: Vec<TemplateStruct>,
    /// Methods with the name of the type their `impl` block is for.
    methods: Vec<(String, Member)>,
}

/// Each Rust file's modification time when it was read, `None` when it could not be read or parsed (so it is tried again next time).
type Signature = Vec<(PathBuf, Option<SystemTime>)>;

/// The facts of each Rust file read so far, by path, and each crate's merged facts.
#[derive(Debug, Default)]
pub struct RustIndex {
    /// Each file's facts, with the modification time they were read at.
    files: HashMap<PathBuf, (Option<SystemTime>, Arc<FileFacts>)>,
    /// Each crate's merged facts, by crate root, with the files they were merged from.
    crates: HashMap<PathBuf, (Signature, Arc<CrateFacts>)>,
    /// Whether the caller watches the Rust files and reports every change (`changed`), so facts can be trusted without looking at the disk.
    watched: bool,
    /// The crates whose merged facts are current: every file was read and parsed, and no change has been reported since. Only while
    /// `watched`.
    current: HashSet<PathBuf>,
}

impl RustIndex {
    /// Says whether the caller watches the Rust files. While it does, a crate's facts are worked out again only after `changed` reports a
    /// change under it; otherwise every call checks each file's modification time.
    pub fn set_watched(&mut self, watched: bool) {
        self.watched = watched;
        self.current.clear();
    }

    /// Reports that a Rust file was created, changed or deleted, so its crate's facts are worked out again on next use.
    pub fn changed(&mut self, file: &Path) {
        self.current.retain(|root| !file.starts_with(root));
        self.files.remove(file);
    }

    /// The facts of every Rust file under the crate's `src`.
    ///
    /// A custom filter is any function inside a module named `filters`, inline (`mod filters { … }`) or in its own file (`filters.rs`,
    /// `filters/mod.rs`). Askama looks filters up in the `filters` module in scope of the template's struct; this reading does not check
    /// which struct can see which module.
    ///
    /// A file that cannot be read or parsed, as while it is being saved, keeps the facts last read from it, so its structs and filters do
    /// not vanish for a moment.
    pub fn crate_facts(&mut self, crate_root: &Path) -> Arc<CrateFacts> {
        if self.current.contains(crate_root)
            && let Some((_, facts)) = self.crates.get(crate_root)
        {
            return Arc::clone(facts);
        }
        let src = crate_root.join("src");
        let files = rust_files(&src);
        let listed: HashSet<&Path> = files.iter().map(PathBuf::as_path).collect();
        self.files.retain(|path, _| !path.starts_with(&src) || listed.contains(path.as_path()));
        let mut signature = Signature::with_capacity(files.len());
        let mut read = Vec::with_capacity(files.len());
        for file in files {
            let modified = std::fs::metadata(&file).and_then(|metadata| metadata.modified()).ok();
            let cached = self.files.get(&file);
            let facts = match cached {
                Some((when, facts)) if *when == modified && modified.is_some() => Arc::clone(facts),
                _ => match read_file(&file) {
                    Some(facts) => {
                        let facts = Arc::new(facts);
                        self.files.insert(file.clone(), (modified, Arc::clone(&facts)));
                        facts
                    }
                    None => {
                        // Kept, with no time, so the next request reads the file again.
                        let facts = cached.map_or_else(|| Arc::new(FileFacts::default()), |(_, facts)| Arc::clone(facts));
                        self.files.insert(file.clone(), (None, Arc::clone(&facts)));
                        facts
                    }
                },
            };
            let when = self.files.get(&file).and_then(|(when, _)| *when);
            signature.push((file, when));
            read.push(facts);
        }
        let complete = signature.iter().all(|(_, when)| when.is_some());
        // A file that could not be read or parsed is read again next time, so the crate is not current until it can be.
        if self.watched && complete {
            self.current.insert(crate_root.to_path_buf());
        }
        if let Some((known, facts)) = self.crates.get(crate_root)
            && *known == signature
            && complete
        {
            return Arc::clone(facts);
        }
        let facts = Arc::new(merge(&read));
        self.crates.insert(crate_root.to_path_buf(), (signature, Arc::clone(&facts)));
        facts
    }
}

/// One crate's facts from its files' facts: every file's filters and structs, with each method added to the structs of the type its `impl`
/// block is for.
fn merge(files: &[Arc<FileFacts>]) -> CrateFacts {
    let mut facts = CrateFacts::default();
    let mut methods = Vec::new();
    for read in files {
        facts.filters.filters.extend(read.filters.filters.iter().cloned());
        facts.filters.incomplete |= read.filters.incomplete;
        facts.structs.extend(read.structs.iter().cloned());
        methods.extend(read.methods.iter());
    }
    for (owner, method) in methods {
        for template_struct in facts.structs.iter_mut().filter(|template_struct| template_struct.name == *owner) {
            template_struct.members.push(method.clone());
        }
    }
    facts
}

/// How deep below `src` the search for Rust files goes, against symbolic link cycles.
const MAX_DEPTH: usize = 16;

/// Every `.rs` file under `dir`, sorted by path.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_files(dir, 0, &mut Visited::default(), &mut files);
    files
}

/// Adds the `.rs` files under `dir` to `files`, in path order, going at most `MAX_DEPTH` deep, entering each directory once and stopping at
/// `MAX_WALKED_FILES`. Only regular files count: a named pipe would never finish reading.
fn collect_rust_files(dir: &Path, depth: usize, visited: &mut Visited, files: &mut Vec<PathBuf>) {
    if !visited.enter(dir) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
    entries.sort();
    for path in entries {
        if files.len() >= MAX_WALKED_FILES {
            return;
        }
        // A symbolic link is followed, so a linked directory counts as one and a linked file as a file.
        if path.is_dir() {
            if depth < MAX_DEPTH {
                collect_rust_files(&path, depth + 1, visited, files);
            }
        } else if path.is_file() && path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Reads one Rust file's facts. `None` when it cannot be read or parsed.
fn read_file(file: &Path) -> Option<FileFacts> {
    let source = crate::project::read_text(file)?;
    let parsed = syn::parse_file(&source).ok()?;
    let lines = LineIndex::new(&source);
    let whole_file_is_filters = file.file_stem().is_some_and(|stem| stem == "filters")
        || (file.file_stem().is_some_and(|stem| stem == "mod") && file.parent().and_then(Path::file_name).is_some_and(|dir| dir == "filters"));
    let mut facts = FileFacts::default();
    let file: Arc<Path> = Arc::from(file);
    collect(&parsed.items, whole_file_is_filters, &Reader { file, source: &source, lines: &lines }, &mut facts);
    Some(facts)
}

/// A Rust file being read: its path, its text and its line index, to turn spans into ranges.
struct Reader<'a> {
    /// The file's path, shared by everything read from it.
    file: Arc<Path>,
    /// The file's text.
    source: &'a str,
    /// The line index of `source`.
    lines: &'a LineIndex,
}

impl Reader<'_> {
    /// The byte offset of a span's start or end. `proc-macro2`'s columns count characters.
    fn offset(&self, location: proc_macro2::LineColumn) -> usize {
        let line = u32::try_from(location.line.saturating_sub(1)).unwrap_or(u32::MAX);
        let line_start = self.lines.offset(self.source, Position { line, character: 0 });
        self.source[line_start..].char_indices().nth(location.column).map_or(self.source.len(), |(at, _)| line_start + at)
    }

    /// The byte range of a span.
    fn range(&self, span: proc_macro2::Span) -> Range<usize> {
        self.offset(span.start())..self.offset(span.end())
    }

    /// The source text of a span, with whitespace collapsed.
    fn text(&self, span: proc_macro2::Span) -> String {
        self.source[self.range(span)].split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

/// Adds the filters, template structs and methods among `items` to `facts`, going into inline modules. `in_filters` says that `items` are
/// those of a `filters` module.
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
                                file: Arc::clone(&reader.file),
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

/// The custom filter a function in a `filters` module defines.
fn custom_filter(function: &syn::ItemFn, reader: &Reader<'_>) -> CustomFilter {
    CustomFilter {
        name: function.sig.ident.to_string(),
        signature: reader.text(function.sig.span()),
        docs: docs(&function.attrs),
        file: reader.file.to_path_buf(),
        range: reader.range(function.sig.ident.span()),
    }
}

/// A struct that derives `Template` and names a template file. One with an inline `source = "..."` template, or none, is not one.
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
                    file: Arc::clone(&reader.file),
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
        file: Arc::clone(&reader.file),
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
    fn trusts_watched_facts_until_a_change_is_reported() {
        let dir = TempDir::new();
        dir.write("src/main.rs", "#[derive(Template)]\n#[template(path = \"a.html\")]\nstruct A { x: u8 }\n");
        let mut index = RustIndex::default();
        index.set_watched(true);
        assert_eq!(index.crate_facts(&dir.path("")).structs[0].members.len(), 1);
        // A change no one reports is not seen: the file system is not looked at.
        std::thread::sleep(std::time::Duration::from_millis(20));
        dir.write("src/main.rs", "#[derive(Template)]\n#[template(path = \"a.html\")]\nstruct A { x: u8, y: u8 }\n");
        assert_eq!(index.crate_facts(&dir.path("")).structs[0].members.len(), 1);
        index.changed(&dir.path("src/main.rs"));
        assert_eq!(index.crate_facts(&dir.path("")).structs[0].members.len(), 2);
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
