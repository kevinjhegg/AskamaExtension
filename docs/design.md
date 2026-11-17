# Askama extension design

This document describes the extension as it is built today and the architecture every step of `implementation-plan.md` builds on. Bring it up to date with each step.

## Goal

Give Askama templates the editing help Rust code already has in VS Code: highlighting, syntax errors as you type, navigation between templates, and completion that knows the template's Rust struct and, eventually, the types of its fields.

The target is Askama 0.16, the version in use. Templates are HTML with htmx attributes, so the extension must keep VS Code's own HTML support working in the same file.

## Shape

The extension has two parts.

| Part | Path | Language | Role |
|---|---|---|---|
| Client | `editors/vscode` | TypeScript | Starts the server, says which files are templates, contributes the grammar, snippets and settings, and adds Askama editing in templates (comment toggling, auto-closing, Surround with Tag). Holds no language analysis. |
| Server | `server` (`askama-lsp`) | Rust | Every language feature, over the Language Server Protocol on stdin and stdout. |

The split follows rust-analyzer's own: a thin client and a Rust server. The server can parse templates with Askama's own parser crate (`askama_parser`) and read Rust with `syn`, which TypeScript could not do without reimplementing both. It also keeps the door open to other editors, since any LSP client can start `askama-lsp`.

`fixtures/sample-site` is a small Askama crate the Extension Development Host opens. It covers inheritance, includes, macros, a custom filter, loops and htmx attributes, so each step has something to try.

## Which files are templates

Templates keep the `html` language. The client does not invent an `askama` language, because that would switch off VS Code's HTML features (tag completion, Emmet, formatting) in the very files that are mostly HTML. Instead:

- The grammars are injections into `text.html.basic` and `text.html.derivative`, so Askama tags are colored on top of HTML (see "Highlighting").
- The client registers the server for files matching `askama.templateGlobs` (default `**/templates/**`), so the server sees only templates.
- From step 3 the server works out each template's crate, its template directories from `askama.toml` (default `templates`), and the template's name relative to them, which is the name `#[template(path = "...")]`, `{% extends %}` and `{% include %}` use.

## Highlighting

Coloring is done by two TextMate injection grammars in the client, with no help from the server.

`syntaxes/askama.injection.tmLanguage.json` has the selector `L:text.html` minus its own regions (Askama comments, raw blocks, expressions and statements), so it applies anywhere in an HTML file, including inside attribute values, `<script>` and HTML comments, where Askama also expands tags. `L:` puts it ahead of HTML's own rules, which matters where HTML would otherwise claim the text, such as an unquoted attribute value or `{% if %}` between attributes. Inside an Askama tag it uses its own rules only. The scopes are chosen so the default themes give each kind of token a distinct color:

| Token | Scope |
|---|---|
| Delimiters | `punctuation.section.embedded.{begin,end}.askama` |
| Tag keywords | `keyword.control.askama` |
| Block name | `entity.name.type.block.askama` |
| Macro name | `entity.name.function.macro.askama` |
| Filter | `support.function.filter.askama` |
| Path segment before `::` | `entity.name.namespace.askama` |
| Function and method calls | `entity.name.function.askama`, `entity.name.function.member.askama` |
| Variable, field | `variable.other.askama`, `variable.other.property.askama` |
| `loop`, `self`, `super`, `caller` | `variable.language.askama` |
| Comment | `comment.block.askama` |

`syntaxes/htmx.injection.tmLanguage.json` applies inside HTML start tags (`L:meta.tag`, outside attributes). It matches `hx-*` and `data-hx-*` attribute names ahead of HTML's generic attribute rule, gives them `entity.other.attribute-name.html entity.name.type.htmx` so themes color them unlike other attributes (the Dark+ family colors `support.function` the same as attribute names in some themes, so that was not used), and reuses HTML's own `attribute-interior` for the value. The value of `hx-on*` is embedded JavaScript (`meta.embedded.inline.js source.js`, mapped to the `javascript` language).

Snippets for the block tags are contributed for the `html` language.

`npm test` checks the grammars with `vscode-tmgrammar-test` against VS Code's own HTML grammar, vendored with a README in `test/grammar/vendor`.

## Editing in templates

Templates keep the `html` language, so VS Code's HTML support and every extension keyed to `html` (Emmet, Tailwind, htmx helpers, formatters) keep working in them. The cost is that a language configuration, VS Code's usual way to define comment toggling, auto-closing pairs and indentation rules, cannot be used: there is one per language, and an Askama one for `html` would change every HTML file. (The Askama Templates extension, `weixu.wx-vsce-askama-templates`, takes the other road: a language of its own per host format, which loses all of that.) The client adds the conveniences itself, limited to templates:

- **Askama: Toggle Comment** wraps lines in `{# … #}` or unwraps them. Its Cmd+/ (Ctrl+/) keybinding applies only when `editorLangId == html` and the path has a `templates` directory in it; elsewhere Cmd+/ is VS Code's own. The command also checks that the file matches `askama.templateGlobs` and falls back to VS Code's line comment if not.
- Auto-closing is a listener on document changes: after `{%`, `{{` or `{#` is typed in a template, it adds the closer with a space on each side of the cursor, replacing the `}` that HTML's bracket auto-closing put there.
- **Askama: Surround with Tag** wraps a selection, or whole lines, in a block tag pair, as a snippet.
- Indentation after block tags is not provided: it would need HTML's language configuration replaced for every HTML file.

The text transformations live in `src/editing.ts`, apart from the VS Code API, and are unit tested with Node's test runner.

## Server internals

The analysis lives in a library crate, `analysis/` (`askama-analysis`), that knows nothing about LSP, so it is unit tested on strings and temporary directories. The server crate, `server/` (`askama-lsp`), only translates between LSP and that library.

The analysis grows in layers, each usable on its own:

1. **Syntax** (built). Two readers of the template, both using the crate's syntax from `askama.toml`:
   - `askama_parser` (`analysis/src/template.rs`) decides whether the template is valid, so syntax errors match the compiler's exactly.
   - A tag scanner (`analysis/src/tags.rs`) finds every tag's exact range, and `analysis/src/outline.rs` pairs openers with closers into a tree of regions. Outline, folding and selection ranges come from that tree. `askama_parser`'s tree records only where each tag's keyword is, not where a block ends, and gives nothing for a template with an error, so the scanner is what keeps the outline working while a template is mid-edit.

   Navigation between templates (`analysis/src/navigation.rs`) reads template paths, block and macro names and `{% call %}` targets out of the same tags. Filters (`analysis/src/filters.rs`) are found in the same tags, and completion (`analysis/src/completion.rs`) decides what to offer from the text before the cursor.
2. **Rust index** (built). `syn` reads the crate's source files (`analysis/src/rust.rs`) to find each `#[derive(Template)]` struct, the template it names, its fields and methods, and the crate's `filters` modules. That gives completion, hover and navigation for a template's top-level variables and custom filters, with types as written and nothing resolved.
3. **Types** (built). For `{{ order.customer.` and loop variables the server needs real Rust type resolution. It starts its own rust-analyzer process for the crate and asks it through a probe (below).

### What the server does today

- It keeps the text and version of each open template, with full text synchronization.
- On open and on every change it checks the template and publishes its diagnostics with the document's version. Closing a template clears them.
- Checking a template: find its crate (the nearest directory above it with a `Cargo.toml`), get the crate's configuration, parse with the crate's default syntax, and turn a parse error into one error diagnostic (source `askama`) on the word where Askama stopped. The parser stops at the first mistake, so a template has at most one syntax error at a time.
- The configuration is read from `askama.toml` with the rules of `askama_derive` 0.16 (`analysis/src/config.rs`) and cached per crate. A broken `askama.toml` is reported on that file, on the offending value where it can be found, and its templates are checked with Askama's defaults meanwhile.
- If the client supports dynamic registration of `workspace/didChangeWatchedFiles` (VS Code does), the server registers a watcher for `**/askama.toml` after initialization. A change drops that crate's cached configuration and rechecks every open template.
- All offset conversion is in `analysis/src/lines.rs`. Columns are UTF-16 code units, the LSP default, which VS Code uses.
- `askama_parser` runs inside `catch_unwind`, so a parser panic costs one diagnostic, not the server.
- It answers `textDocument/documentSymbol` (the region tree, with `extends`, `import` and `include` as leaves), `textDocument/foldingRange` (one fold per branch of each region, ending on the line before the next branch or the closing tag, and one per multi-line comment) and `textDocument/selectionRange` (word, tag content, tag, then each enclosing region's body and whole, then the file). Each request rescans the open text; scanning is linear and templates are small, so nothing is cached.
- VS Code combines these with its own HTML answers: the Outline shows both, folds from both apply, and Expand Selection merges both chains.
- Navigation across templates goes through `navigation::Project`, built per request from the crate's configuration and a reader that returns an open document's unsaved text, or the file on disk otherwise. There is no index of templates: each question reads the templates it needs, which for a crate's handful of small templates is quick and can never be stale.
  - Template paths resolve like Askama's `find_template`: beside the naming template first, then each template directory in order.
  - A block's definition is the same-named block in the nearest template up the `extends` chain. Its references and implementations are the same-named blocks in every template whose `extends` chain reaches it. The chain stops after 32 templates or on a cycle.
  - A called macro's definition is in the imported template for `m::name`, otherwise in the same template or one it extends.
  - `textDocument/documentLink` links each path that exists, and the check on open and change adds a warning on each path that does not.
  - `workspace/symbol` lists the blocks and macros of every crate found under the workspace folders (`project::crates_under`) and of every crate with an open template.
- The Rust index (`rust::RustIndex`) reads every Rust file under a crate's `src` with `syn` and keeps each file's result until its modification time changes. Rust files are read from disk, so a change counts once saved. It yields the crate's custom filters and its template structs: each struct that derives `Template` with `#[template(path = "...")]`, the template's name and where it is written, its named fields (type as written, doc comment) and the methods with a `self` receiver from its inherent `impl` blocks anywhere in the crate.
- A template's structs (`Project::structs_for`) are those whose template is it or reaches it through `extends` or `include`; `import` does not count, since macros do not see the struct's fields. Its variables are the union of their members; a member only some of them have is sorted after the rest and labeled with the structs that have it.
- `navigation::variable_at` decides whether the identifier at a position is a variable: not after `.`, `::` or `|`, not before `::`, not a tag keyword, not in a literal, and not in tags that name something else (`block`, `macro`, `call`, `filter`, template paths).
- Names the template binds (`analysis/src/scopes.rs`): `for` patterns and `loop` over the loop body (not its `else`), `let` and `set` to the end of their branch or block (the block form after its `endlet`), macro arguments over the macro body, and `when`, `if let`, `elif let` and `else if let` patterns over their arm or branch. Patterns are read by rule, not parsed: a binding is a lowercase identifier that is not a path segment, a constructor, a struct name or a field label. The innermost binding of a name wins.
  - Completion lists them before the struct's members, nearest first. After `loop.` it offers `loop`'s members; after any other `name.` it offers nothing yet.
  - Hover says what bound a name; definition goes to where it is bound; rename (with `textDocument/prepareRename`) and references cover the binding and its uses, skipping uses of a shadowing binding.
  - The check warns on a variable that is neither bound nor a member of the template's structs, but only where that is certain: the template has structs and is their own template or one they extend (`Project::sees_only_struct_names`). An included template can also use the names bound around its `include`, so it is not checked. A variable is an identifier in an expression part (`variable_uses`) that is not a syntax word, not called (`f(`, `m!`), not uppercase and not a named argument (`width = 3` in a macro call).
- Requests and notifications run concurrently in tower-lsp, so `did_open` and `did_change` store the document before their first `.await`; a request sent right after `didOpen` then finds it. For the same reason checks can finish out of order: a check whose document has changed since it started does not publish, so stale diagnostics never land on text that has moved.
- While a template has a syntax error, the check reports only that error. An unclosed tag swallows the rest of the file, so names "found" after it are not names, and unknown-variable and unknown-filter warnings would be noise. Missing-template warnings are still reported.
- Member completion (`server/src/analyzer.rs`, `analysis/src/probe.rs`): after `name.` or `name.more.`, where `name` is a member of the template's struct, the server writes the probe described above into the struct's Rust file as read from disk, right after the struct's item, and asks the crate's private rust-analyzer for completion at its end. It keeps fields and methods only (a method inserted with `()`), drops anything needing an import, and puts rust-analyzer's type, documentation and order on each item.
  - rust-analyzer is found by the client: `askama.rustAnalyzer.path`, else the executable the rust-analyzer extension ships, else `rust-analyzer` on the PATH (often only rustup's proxy, which fails without its component). `askama.typeCompletion` turns the feature off. Both are initialization options, so a change needs a server restart.
  - One rust-analyzer per crate, started on first use, initialized to watch files itself, never run `cargo check` on save, run its own cargo commands in a separate target directory (`cargo.targetDir`), and offer no postfix snippets or auto-imports. It is ready when it reports itself quiescent. Its own requests get empty answers. When it exits, everything waiting on it fails at once. It exits with the server, since its input closes.
  - A completion waits up to 3 seconds for the crate to load and otherwise answers nothing; the client shows progress until rust-analyzer is ready, when the user asks again.
- Names the template binds get their types the same way (`analysis/src/lowering.rs`). The probe writes the template's constructs around the cursor as `askama_derive` 0.16 generates them: a `for` loop as `for p in ITER {` with Askama's choice of `ITER` (`e` for a range, `e.iter()` for an array, `(e).into_iter()` for a call, index or local, `(&e).into_iter()` for a field), `let p = &(e);` (no `&` for a local or literal), `if let p = e {`, a `when` arm as `if let p = &(e) {`, and a macro argument as `let a = &(v);`. Names the template does not bind become `self.name`. Only constructs whose scope holds the cursor are written. Askama-only syntax is made Rust of the same length or cut: a final filter chain is cut, so the filtered value stands for the expression (`tab.grid|safe` is `tab.grid`), and `~` becomes `+`, which keeps its operands' types checked and their positions mapped. A filter inside brackets, or `is defined`, cannot be written, and then nothing is offered.
  - Inside a macro, the probe starts at the macro's one `{% call %}` in any template (`Project::macro_call_sites`): the caller's constructs at the call, the arguments as Askama passes them, then the macro body's constructs. A macro called from more than one place offers nothing.
- Hover and go to definition on a name after `.` (`customer` in `order.customer.name`) ask rust-analyzer at that name in a probe line `let _ = &(path);`. A definition in the probed file is moved back to its line on disk (`Probe::original_line`); one inside the probe itself is dropped.
- Type errors: after each check, a quarter second later if no newer edit came, every expression of the template goes into one probe, each in its own block with its own scope (`probe::with_blocks`), and rust-analyzer's diagnostics for it are pulled (`textDocument/diagnostic`). Errors with the codes for no such field or method, the wrong argument count or a private member (rustc's `E0559`, `E0609`, `E0599`, `E0061`, `E0616`, `E0624`, and rust-analyzer's own names for them) on an expression's line are mapped back through `lowering::Mapped`. These checks are rust-analyzer's experimental diagnostics, which the private instance turns on. Left out: expressions with a filter inside brackets, `is defined` or `loop`, and those in macros. The type mismatches that writing `~` as `+` can cause, and errors on the unresolved paths of macro calls in expressions, are not among the codes reported. A type check runs only where unknown variables are checked, when the template has no syntax error, and when the crate's rust-analyzer is already running and ready; it is never started for this. The last type errors stay in later checks' publications until the next type check replaces them.
- All probes of a crate replace the same file in rust-analyzer's memory, so `Analyzer::ask` holds a lock from showing a probe to receiving the answer.
- Completion triggers: `|`, a space and `.`. VS Code asks for completions unprompted only on a letter or a trigger character, so without the space nothing would open after `{{ `, and without `.` nothing after `loop.`. On a space the server answers only where a name belongs (`completion::after_space`): right after `{{` or `{%`, after `|`, after `if`, `elif`, `in`, `match`, `when` or `filter`, and after an operator, `(`, `[` or `,`.
- Variable features: completion in expressions, hover with the member's type or signature and doc comment, and definition to the field or method in Rust.
- Rust files: the client sends them too (its document selector adds the `rust` language), and the server marks a document opened with language `rust` as Rust. Rust documents are never checked or answered as templates. In them the server gives an **Open template** code lens above each template struct, which runs the client's `askama.openTemplate` command, and goes from the `path = "..."` string to the template. Positions in Rust files come from the file on disk.
- Filters:
  - Built-ins are a table of the 35 names `askama_derive` 0.16 dispatches on (`filters::BUILTINS`).
  - Custom filters are every function in a module named `filters` in any Rust file under the crate's `src`, inline or as `filters.rs` or `filters/mod.rs`, read with `syn` (with `proc-macro2`'s `span-locations`, so signatures and names can be located in the file). They come from the Rust index.
  - A `use` inside a `filters` module makes the list incomplete, which turns off the unknown-filter warning for that crate, since the name may come from elsewhere.
  - `textDocument/completion` offers the crate's filters, then the built-ins, after `|` (a trigger character) or `{% filter`. At the start of a `{% %}` tag it offers the keywords, with the closing tag of the innermost open region (from the outline of the text before the tag) sorted first and preselected. Nothing is offered inside a string or char literal.
  - `textDocument/hover` on a filter shows a custom filter's signature and doc comment, or a built-in's usage and description. `textDocument/definition` on a custom filter goes to its Rust function.
  - The check on open and change warns on a filter that is neither built in nor custom. A path-qualified filter is never flagged.

### The probe

Askama generates its code inside a derive macro, and rust-analyzer does not map that code back to template positions, so it cannot be asked about a template directly. The probe works around that:

1. The server turns the expression before the cursor, and the template scopes around it, into Rust. For `{{ order.customer.` inside `{% for item in order.items %}` in the template of `OrderPage`, that is roughly:

   ```rust
   impl OrderPage {
       fn __askama_probe(&self) {
           for item in &self.order.items {
               self.order.customer.
           }
       }
   }
   ```

2. It appends that to the in-memory text of the file that defines `OrderPage`, sends the edited text to its private rust-analyzer as an unsaved change, and asks for completion at the end of the probe.
3. It translates the answers back to template positions.

The probe is never written to disk. Nothing reaches the build or git, and the user's own rust-analyzer and editor buffers are untouched, because the server talks to a rust-analyzer process of its own.

Trade-offs, accepted deliberately:

- A second rust-analyzer costs memory and indexing time. It starts only when type-aware completion is first needed, and the setting `askama.typeCompletion` turns it off.
- Reusing the user's rust-analyzer through VS Code's `vscode.executeCompletionItemProvider` was rejected. It needs a real document in the crate's module tree, which means either a file on disk or an edit to the user's open buffer.
- How Askama lowers each construct (for example what a `for` loop iterates over) is copied from `askama_derive`'s generated code for the pinned version and checked by tests against the fixture crate.

## Versions

| Component | Version |
|---|---|
| Askama (fixture, and the parser the server pins) | 0.16 |
| `tower-lsp-server` | 0.23 |
| rust-analyzer (tested) | 0.3.3121 |
| `vscode-languageclient` | 11 |
| VS Code engine | 1.146 or later |

The server pins `askama_parser` to the exact version the fixture uses. A project on another Askama release may see small differences in what counts as a syntax error. Matching the parser to the project's own `Cargo.lock` is a possible later improvement.

## Known limits

- The TextMate grammar knows only the default delimiters (`{{ }}`, `{% %}`, `{# #}`). A project that sets other delimiters in `askama.toml` gets correct diagnostics and navigation from the server, but not correct coloring. Semantic tokens from the server could fix that later.
- The grammar is injected into every HTML file, not only templates. `{%` and `{{` are rare in plain HTML, so this is accepted.
- Templates given inline with `#[template(source = "...")]` are out of scope.
- Every template is checked with its crate's default syntax. A struct that names another syntax with `#[template(syntax = "...")]` is not yet taken into account.
- Template structs with an inline `source`, enums that derive `Template`, and fields of tuple structs are not read.
- Type errors appear only after rust-analyzer has been started for the crate by a member completion or a hover on a member.
- Member completion follows dotted paths only, not method calls in the path (`order.items.first().`). Members of a macro argument are known only when the macro is called from exactly one place. It reads the struct's Rust file from disk, so unsaved edits to it are not seen.
- Names bound in a template are known by name only, not type. Names a `{% call %}` block's `caller` arguments bind, and names an including template passes to an included one, are not tracked.
- Custom filters are found in any `filters` module of the crate, not only the one in scope of the template's struct.
- Warnings that depend on other files (missing templates, unknown filters) are recomputed when the template itself is opened or edited, not when the other file changes.
- Macros are found from `{% call %}` tags only. A macro called as an expression is not yet followed.
- `askama.toml` is read from the crate root only. A struct's `config = "..."` attribute, naming another configuration file, is not followed.

## Building and running

- `cargo build -p askama-lsp` builds the server into `target/debug/askama-lsp`. `cargo test --workspace` runs the analysis unit tests and the end-to-end tests, which start that binary and talk LSP to it.
- `npm install` and `npm run compile` in `editors/vscode` build the client. `npm test` there runs the grammar tests and the editing unit tests (compile first).
- In VS Code, open this repository and run **Run Askama extension** (F5). It builds both, then opens `fixtures/sample-site` in an Extension Development Host.
- `sh scripts/package.sh` builds a VSIX for this machine's platform at the repository root: a release build of the server in `server/`, and the client bundled by esbuild into `out/extension.js`. Install it with **Extensions: Install from VSIX…**.
- The client finds the server through, in order: the `askama.server.path` setting, then `server/askama-lsp` in the extension (a packaged extension) and `target/debug/askama-lsp` (this checkout), the latter first when run from the checkout (F5).
- **Askama: Restart Language Server** restarts the server after a rebuild without reloading the window.
