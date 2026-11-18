# Askama extension design

This document describes the extension as it is built today and its architecture. Bring it up to date with each change.

## Goal

Give Askama templates the editing help Rust code already has in VS Code: highlighting, syntax errors as you type, navigation between templates, and completion that knows the template's Rust struct and the types of its fields.

The target is Askama 0.16, the version in use. Templates are HTML with htmx attributes, so the extension must keep VS Code's own HTML support working in the same file.

## Shape

The extension has two parts.

| Part | Path | Language | Role |
|---|---|---|---|
| Client | `editors/vscode` | TypeScript | Starts the server, says which files are templates, contributes the grammar, snippets and settings, and adds Askama editing in templates (comment toggling, auto-closing, Surround with Tag). Holds no language analysis. |
| Server | `server` (`askama-lsp`) | Rust | Every language feature, over the Language Server Protocol on stdin and stdout. |

The split follows rust-analyzer's own: a thin client and a Rust server. The server can parse templates with Askama's own parser crate (`askama_parser`) and read Rust with `syn`, which TypeScript could not do without reimplementing both. It also keeps the door open to other editors, since any LSP client can start `askama-lsp`.

`fixtures/sample-site` is a small Askama crate the Extension Development Host opens. It covers inheritance, includes, macros, a custom filter, loops and htmx attributes, so each feature has something to try.

## Which files are templates

Templates keep the `html` language. The client does not invent an `askama` language, because that would switch off VS Code's HTML features (tag completion, Emmet, formatting) in the very files that are mostly HTML. Instead:

- The grammars are injections into `text.html.basic` and `text.html.derivative`, so Askama tags are colored on top of HTML (see "Highlighting").
- The client registers the server for files matching `askama.templateGlobs` (default `**/templates/**`), so the server sees only templates.
- The server works out each template's crate, its template directories from `askama.toml` (default `templates`), and the template's name relative to them, which is the name `#[template(path = "...")]`, `{% extends %}` and `{% include %}` use.

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
- On open and on every change it checks the template and publishes its diagnostics with the document's version. Closing a template clears them, with everything the server kept for it, and cancels its pending type check.
- Checking a template: find its crate (the nearest directory above it with a `Cargo.toml`), get the crate's configuration, parse with the crate's default syntax, and turn a parse error into one error diagnostic (source `askama`) on the word where Askama stopped. The parser stops at the first mistake, so a template has at most one syntax error at a time.
- The configuration is read from `askama.toml` with the rules of `askama_derive` 0.16 (`analysis/src/config.rs`) and cached per crate. A broken `askama.toml` is reported on that file, on the offending value where it can be found, and its templates are checked with Askama's defaults meanwhile.
- If the client supports dynamic registration of `workspace/didChangeWatchedFiles` (VS Code does), the server registers a watcher for `**/askama.toml` and `**/*.rs` after initialization, and one for each crate's template directories once its configuration is read (see "Performance"). A change to an `askama.toml` drops that crate's cached configuration; a change to a Rust file or a template drops what is known of it. Every change rechecks every open template.
- All offset conversion is in `analysis/src/lines.rs`. Columns are UTF-16 code units, the LSP default, which VS Code uses.
- `askama_parser` runs inside `catch_unwind`, so a parser panic costs one diagnostic, not the server.
- Every request and notification runs under `catch_unwind` too (`server/src/panics.rs`, a wrapper around the whole tower-lsp service). tower-lsp runs handlers inside the future of the server's main loop, so without it a panic in any of them would end the process. A request that panics is answered with an internal error, the panic is logged in the Askama output channel, and the server goes on. What a handler changed in the shared state before it panicked stays: the changes are single inserts and removes, and the lock ignores poisoning. Spawned tasks such as type checks need no wrapper, since a panic ends only that task.
- It answers `textDocument/documentSymbol` (the region tree, with `extends`, `import` and `include` as leaves), `textDocument/foldingRange` (one fold per branch of each region, ending on the line before the next branch or the closing tag, and one per multi-line comment) and `textDocument/selectionRange` (word, tag content, tag, then each enclosing region's body and whole, then the file). Each request rescans the open text; scanning is linear (each delimiter is searched for again only once the scan has passed where it was last found, so one the template never uses costs one search) and templates are small, so nothing is cached.
- The rules for reading the Rust-like text inside tags (the words and operators outside brackets and literals: `in`, `=`, `,`, `:`, `if let`) live in one module, `analysis/src/text.rs`, which the outline, the scopes and the lowering share, so all three read a tag the same way. Lookups of the tag at an offset are binary searches, since tags are in order and do not overlap.
- VS Code combines these with its own HTML answers: the Outline shows both, folds from both apply, and Expand Selection merges both chains.
- Navigation across templates goes through `navigation::Project`, built per request from the crate's configuration and a reader that returns an open document's unsaved text, or the file on disk otherwise. There is no index of templates: each question reads the templates it needs. What that takes from the file system (texts, canonical paths, where each template name resolves, the templates each one reaches, the list of template files) is kept in a `navigation::FileCache`: one per crate across requests while its template directories are watched, otherwise one per request. Structs naming the same template are followed once, and a base template many structs share is read and scanned once.
  - Template paths resolve like Askama's `find_template`: beside the naming template first, then each template directory in order.
  - A block's definition is the same-named block in the nearest template up the `extends` chain. Its references and implementations are the same-named blocks in every template whose `extends` chain reaches it. The chain stops after 32 templates or on a cycle.
  - A called macro's definition is in the imported template for `m::name`, otherwise in the same template or one it extends.
  - `textDocument/documentLink` links each path that exists, and the check on open and change adds a warning on each path that does not.
  - `workspace/symbol` lists the blocks and macros of every crate found under the workspace folders (`project::crates_under`) and of every crate with an open template. The editor asks on every keystroke of the query, so the crates found are reused for 10 seconds.
- The Rust index (`rust::RustIndex`) reads every Rust file under a crate's `src` with `syn` and keeps each file's result until it changes: while Rust files are watched, until a change is reported (nothing is looked at on disk); otherwise until its modification time changes, and each crate's merged result until one of its files changes, so a request that finds nothing changed shares the last result rather than copying it. A file that cannot be read or parsed, as while it is being saved, keeps its last result, so its structs and filters do not vanish for a moment; it is read again on the next request. The search for Rust files goes at most 16 directories below `src`, and the search for templates as deep below each template directory, against symbolic link cycles. Rust files are read from disk, so a change counts once saved. It yields the crate's custom filters and its template structs: each struct that derives `Template` with `#[template(path = "...")]`, the template's name and where it is written, its named fields (type as written, doc comment) and the methods with a `self` receiver from its inherent `impl` blocks anywhere in the crate.
- A template's structs (`Project::structs_for`) are those whose template is it or reaches it through `extends` or `include`; `import` does not count, since macros do not see the struct's fields. Its variables are the union of their members; a member only some of them have is sorted after the rest and labeled with the structs that have it.
- `navigation::variable_at` decides whether the identifier at a position is a variable: not after `.`, `::` or `|`, not before `::`, not a tag keyword, not in a literal, and not in tags that name something else (`block`, `macro`, `call`, `filter`, template paths).
- Names the template binds (`analysis/src/scopes.rs`): `for` patterns and `loop` over the loop body (not its `else`), `let` and `set` to the end of their branch or block (the block form after its `endlet`), macro arguments over the macro body, a call block's caller parameters (`user` in `{% call(user) list(users) %}`, given their values by the macro's `caller(...)`) over the call's body, and `when`, `if let`, `elif let` and `else if let` patterns over their arm or branch. The name of a called macro, and its arguments, are read after the caller parameters (`text::call_parts`). Patterns are read by rule, not parsed: a binding is a lowercase identifier that is not a path segment, a constructor, a struct name or a field label. The innermost binding of a name wins.
  - Completion lists them before the struct's members, nearest first. After `loop.` it offers `loop`'s members; after any other `name.` it offers the members of the name's type, through the probe (see `lowering.rs` below).
  - Hover says what bound a name; definition goes to where it is bound; rename (with `textDocument/prepareRename`) and references cover the binding and its uses, skipping uses of a shadowing binding.
  - The check warns on a variable that is neither bound nor a member of the template's structs, but only where that is certain: the template has structs and is their own template or one they extend (`Project::sees_only_struct_names`). An included template can also use the names bound around its `include`, so it is not checked. A variable is an identifier in an expression part (`variable_uses`) that is not a syntax word, not called (`f(`, `m!`), not uppercase and not a named argument (`width = 3` in a macro call).
- Requests and notifications run concurrently in tower-lsp, so `did_open` and `did_change` store the document before their first `.await`; a request sent right after `didOpen` then finds it. For the same reason checks can finish out of order: a check whose document has changed since it started does not publish or record its results, so stale diagnostics never land on text that has moved.
- The requests share one state behind one lock, and the Rust index has a lock of its own. A request holds the state lock only to copy out what it needs (a document's text, a crate's configuration) and never while it reads files or parses Rust, so a slow request does not hold up the others. Templates that are not open are read from disk without the lock.
- While a template has a syntax error, the check reports only that error. An unclosed tag swallows the rest of the file, so names "found" after it are not names, and unknown-variable and unknown-filter warnings would be noise. Missing-template warnings are still reported.
- Member completion (`server/src/analyzer.rs`, `analysis/src/probe.rs`): after a chain and a `.`, the chain being names, `.`s, calls and indexing (`order.`, `order.customer.`, `order.items.first().`, `order.items[i].`, found by `text::chain_start`), whose first name is a member of the template's struct or a name the template binds, the chain is lowered as any expression is (so a name in a call's arguments becomes `self.name` where the struct has it), and the server writes the probe described above into the struct's Rust file as read from disk, right after the struct's item, and asks the crate's private rust-analyzer for completion at its end. It keeps fields and methods only (a method inserted with `()`), drops anything needing an import, and puts rust-analyzer's type, documentation and order on each item.
  - rust-analyzer is found by the client: `askama.rustAnalyzer.path` (from user settings only, see "Security"), else the executable the rust-analyzer extension ships, else `rust-analyzer` on the PATH (often only rustup's proxy, which fails without its component). `askama.typeCompletion` turns the feature off. Both are initialization options, so a change needs a server restart.
  - One rust-analyzer per crate, started on first use, initialized to watch files itself, never run `cargo check` on save, run its own cargo commands in a separate target directory (`cargo.targetDir`), and offer no postfix snippets or auto-imports. It is ready while it reports itself quiescent, and not while it reloads the workspace after a change to `Cargo.toml`. Its own requests get empty answers, and its error answers count as no answer. When it exits, everything waiting on it fails at once. It exits with the server, since its input closes, and is stopped after ten minutes in which nothing asked it anything (see "Performance"). It loads no test, bench or example targets (`cargo.allTargets: false`).
  - Every message to it is framed whole (header and body) and queued for one writer task, so a request cancelled part way, as editors cancel stale completions all the time, never leaves half a message on its input, and the task reading its output never waits on its input. A document's version is counted in the same step that queues its change, and a cancelled request's waiter is removed.
  - The progress shown while it loads runs in a task of its own, so the requests waiting for the analyzer do not also wait on the client.
  - A completion waits up to 3 seconds for the crate to load and otherwise answers nothing; the client shows progress until rust-analyzer is ready, when the user asks again.
- Names the template binds get their types the same way (`analysis/src/lowering.rs`). The probe writes the template's constructs around the cursor as `askama_derive` 0.16 generates them: a `for` loop as `for p in ITER {` with Askama's choice of `ITER` (`e` for a range, `e.iter()` for an array, `(e).into_iter()` for a call, index or local, `(&e).into_iter()` for a field), `let p = &(e);` (no `&` for a local or literal), `if let p = e {`, a `when` arm as `if let p = &(e) {`, and a macro argument as `let a = &(v);`. Names the template does not bind become `self.name`. Only constructs whose scope holds the cursor are written. Askama-only syntax is made Rust of the same length or cut: a final filter chain is cut, so the filtered value stands for the expression (`tab.grid|safe` is `tab.grid`), and `~` becomes `+`, which keeps its operands' types checked and their positions mapped. A filter inside brackets, or `is defined`, cannot be written, and then nothing is offered.
  - Inside a macro, the probe starts at the macro's one `{% call %}` in any template (`Project::macro_call_sites`): the caller's constructs at the call, the arguments as Askama passes them, then the macro body's constructs. A macro called from more than one place offers nothing.
- Hover and go to definition on a name after `.` (`customer` in `order.customer.name`, or `name` in `order.items.first().name`) ask rust-analyzer at that name in a probe line `let _ = &(chain);`, the chain lowered up to the name and through it, so the name's place in the lowered code is known. A definition in the probed file is moved back to its line on disk (`Probe::original_line`); one inside the probe itself is dropped.
- Type errors: each open and each change schedules a type check a quarter second later, in a task of its own, so the notification is answered at once and does not take one of tower-lsp's request slots. The next edit cancels it, so a burst of edits gets one type check, after it. The type check puts every expression of the template into one probe, each in its own block with its own scope (`probe::with_blocks`), and rust-analyzer's diagnostics for it are pulled (`textDocument/diagnostic`). Errors with the codes for no such field or method, the wrong argument count or a private member (rustc's `E0559`, `E0609`, `E0599`, `E0061`, `E0616`, `E0624`, and rust-analyzer's own names for them) on an expression's line are mapped back through `lowering::Mapped`. These checks are rust-analyzer's experimental diagnostics, which the private instance turns on. Left out: expressions with a filter inside brackets, `is defined` or `loop`, and those in macros. The type mismatches that writing `~` as `+` can cause, and errors on the unresolved paths of macro calls in expressions, are not among the codes reported. A type check runs only where unknown variables are checked, when the template has no syntax error, and when the crate's rust-analyzer is already running and ready; it is never started for this. The last type errors stay in later checks' publications until the next type check replaces them.
- All probes of a crate replace the same file in rust-analyzer's memory, so `Analyzer::ask` holds a lock from showing a probe to receiving the answer.
- Completion triggers: `|`, a space and `.`. VS Code asks for completions unprompted only on a letter or a trigger character, so without the space nothing would open after `{{ `, and without `.` nothing after `loop.`. On a space the server answers only where a name belongs (`completion::after_space`): right after `{{` or `{%`, after `|`, after `if`, `elif`, `in`, `match`, `when` or `filter`, and after an operator, `(`, `[` or `,`.
- Variable features: completion in expressions, hover with the member's type or signature and doc comment, and definition to the field or method in Rust.
- Rust files: the client sends them too (its document selector adds the `rust` language), and the server marks a document opened with language `rust` as Rust. Only their opening and closing are sent, not their edits: the server reads Rust files from disk, and the client's middleware drops their `didChange`, which carries the whole file on every keystroke. Rust documents are never checked or answered as templates. In them the server gives an **Open template** code lens above each template struct, which runs the client's `askama.openTemplate` command, and goes from the `path = "..."` string to the template. Positions in Rust files come from the file on disk.
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

## Security

The threat considered is a repository the user opens that was written to attack them: its templates, Rust files, `askama.toml` and `.vscode/settings.json`. The extension does not defend against the user's own settings or the programs they choose to run.

- **Workspace trust.** The manifest declares `untrustedWorkspaces: { supported: false }`, so VS Code keeps the extension off in Restricted Mode. What it starts runs the crate's code (below), which only a trusted workspace may do.
- **Programs to run.** `askama.server.path` and `askama.rustAnalyzer.path` are `machine` scoped, which VS Code keeps out of workspace settings, and the client reads only their user value (`inspect(...).globalValue`). A repository's `.vscode/settings.json` cannot choose a program, even in a trusted workspace.
- **The private rust-analyzer** runs build scripts and procedural macros, as every rust-analyzer does, but only when the user's own `rust-analyzer.cargo.buildScripts.enable` and `rust-analyzer.procMacro.enable` allow it: the client passes both, and the server sets them in the private instance's initialization options. It starts only when a template asks about a type, never on opening one.
- **Resource limits**, so a crafted repository cannot exhaust memory, CPU or stack:
  - every file is read through `project::read_text`: regular files only (a named pipe would never finish), at most 8 MiB (`MAX_FILE_SIZE`);
  - the walks of template directories, `src` and the workspace folders enter each directory once by canonical path (`project::Visited`), so symbolic links back to a directory add nothing, go a fixed depth deep and collect at most 10,000 files (`MAX_WALKED_FILES`);
  - a `dirs` pattern in `askama.toml` expands to at most 64 directories (`MAX_TEMPLATE_DIRS`), so `"/**"` stops early;
  - regions nest at most 128 deep (`outline::MAX_NESTING`), which bounds the recursion over the region tree.
- **Panics** in request handlers are caught (see "Server internals"). A stack overflow is not a panic and still ends the process; the nesting limit removes the one known way to cause it, and the client restarts the server.
- **Text from the repository** (doc comments, rust-analyzer's documentation) reaches the editor as LSP Markdown, which the language client renders untrusted: no command links, no HTML. Template paths may name files outside the template directories (`{% include "/etc/x" %}`), as Askama allows; the server only shows the user such a file, as a link or a definition.
- **No network.** The server talks LSP over stdin and stdout and listens on nothing. The probe exists only in the private rust-analyzer's memory. `unsafe` code is forbidden in both crates.
- **Dependencies.** `cargo audit` reports no advisories for the workspace. In the client, `minimatch` is pinned to a patched release with `overrides` (GHSA-g3hv-75wg-m249). `picomatch` (GHSA-fc5q-39xc-vjpf) remains, only in the `vsce` packaging tool, with no fixed release yet. `scripts/package.sh` installs with `npm ci`, so a package is built from exactly what `package-lock.json` holds.

Known gaps: `syn` and `askama_parser` parse recursively, so a Rust file or template nested deeply enough within the 8 MiB limit could still overflow the stack; and every file under a template directory is read as a template, so a large binary file there is read (and rejected as not UTF-8) once per file cache: once until a change in its crate's template directories, or once per request where they are not watched.

## Performance

What a request costs grows with the crate: its templates, its template structs and its Rust files. The server is measured with `tools/lsp_bench.py`, which drives a release build over stdio against a real crate and times each kind of request (`--one-open` opens one page template, as in normal use; `--rust-analyzer PATH` adds the type features). The reference crate is a production web application: 348 templates (850 KB), 365 Rust files (49,000 lines) and 299 template structs. The figures below are medians on an Apple silicon Mac with one template open.

| Request | Before the work below | Now |
|---|---|---|
| Server memory, after use | 17 MB | 16 MB |
| Opening the first template (reads all of the crate's Rust code) | 104 ms | 96 ms |
| Outline, folding, selection, links, going to an `extends` | 0.05 ms | 0.05 ms |
| Variable or filter completion | 14.6 ms | 0.44 ms |
| Checking an edit (time to its diagnostics) | 13.7 ms | 0.46 ms |
| Block references | 14.0 ms | 6.2 ms |
| Workspace symbols | 8.3 ms | 3.1 ms |
| Member completion, warm | 12.3 ms | 1.8 ms |
| Member hover, warm | 10.9 ms | 0.6 ms |
| Type errors after an edit (including the 250 ms delay) | 314 ms | 263 ms |
| Private rust-analyzer ready / its first member completion | 8.8 s / 3 s (empty) | 8.4 s / 3 s (empty) |
| Private rust-analyzer memory | 3.9 GB | 3.9 GB, freed after 10 idle minutes |
| Release binary | 7.6 MB | 3.8 MB |

What the work found and changed:

- **File system work on every request.** Profiling showed that a completion or a check spent nearly all its time asking the file system: reading templates (40%), `canonicalize` (23%), checking every Rust file's modification time (15%) and finding templates (10%), with only a tenth left for scanning tags. The answers do not change between requests unless a file does, so they are kept across requests (`navigation::FileCache`, and the Rust index trusting what it read) and dropped when the editor reports a change. The editor watches `**/*.rs` and each crate's template directories (an LSP relative pattern per directory). A crate is cached only if every template directory exists when it is watched, since a watcher on a directory not yet made would miss its files. A client that cannot watch files gets the earlier behavior: a cache per request and the Rust files' times checked each time.
- **A base template scanned once per struct.** Finding a template's structs followed `extends` and `include` from each struct's template and scanned every template on the way, so a shared base was scanned once per struct (about 250 times per request in the reference crate). Each template's targets are now worked out once and kept with the text they came from.
- **The private rust-analyzer's memory.** It loads the crate's whole Cargo workspace and indexes it up front: 3.9 GB for the reference crate's workspace, more than the user's own rust-analyzer processes together. Turning off indexing up front (`cachePriming`) saved a fifth of that but made the first answer take 11 seconds instead of 3, so it stays on; leaving out test targets saved almost nothing. What helps is not holding the memory while it is not needed: the server stops the private rust-analyzer after ten minutes without a question, and the next member completion or hover starts it again, which takes seconds.
- **Rust file edits.** The client sent every open Rust file's whole text to the server on every keystroke, and the server never used it. It no longer does.
- **The package.** A release profile with link-time optimization, one codegen unit and stripped symbols halves the server binary.

What remains:

- Block references and workspace symbols scan every template on each request (6 and 3 ms for 348 templates). They run when the user asks, not while typing.
- Opening the first template of a crate reads and parses all of its Rust code with `syn` (about 100 ms for 49,000 lines).
- The private rust-analyzer's memory while it runs is rust-analyzer's own, and grows with the workspace.

## Known limits

- The TextMate grammar knows only the default delimiters (`{{ }}`, `{% %}`, `{# #}`). A project that sets other delimiters in `askama.toml` gets correct diagnostics and navigation from the server, but not correct coloring. Semantic tokens from the server could fix that later.
- The grammar is injected into every HTML file, not only templates. `{%` and `{{` are rare in plain HTML, so this is accepted.
- Templates given inline with `#[template(source = "...")]` are out of scope.
- Every template is checked with its crate's default syntax. A struct that names another syntax with `#[template(syntax = "...")]` is not yet taken into account.
- Template structs with an inline `source`, enums that derive `Template`, and fields of tuple structs are not read.
- Type errors appear only while the crate's rust-analyzer runs: after a member completion or a hover on a member has started it, and until it has been idle ten minutes.
- Members of a macro argument are known only when the macro is called from exactly one place. It reads the struct's Rust file from disk, so unsaved edits to it are not seen.
- A call block's caller parameters are known by name only. Their values come from inside the macro (`caller(...)`), so their members are not completed and expressions that use them are not type checked. Names an including template passes to an included one are not tracked.
- Custom filters are found in any `filters` module of the crate, not only the one in scope of the template's struct.
- Warnings that depend on other files (missing templates, unknown filters and variables) are recomputed for every open template when the editor reports a change to a template or a Rust file, which it does when a file is saved. An unsaved edit to another open template does not recompute them, and a client that cannot watch files recomputes them only when the template itself is opened or edited.
- `askama.toml` is read from the crate root only. A struct's `config = "..."` attribute, naming another configuration file, is not followed.

## Building and running

- `cargo build -p askama-lsp` builds the server into `target/debug/askama-lsp`. `cargo test --workspace` runs the analysis unit tests and the end-to-end tests, which start that binary and talk LSP to it.
- `npm install` and `npm run compile` in `editors/vscode` build the client. `npm test` there runs the grammar tests and the editing unit tests (compile first).
- In VS Code, open this repository and run **Run Askama extension** (F5). It builds both, then opens `fixtures/sample-site` in an Extension Development Host.
- `.github/workflows/release.yml` builds and tests every platform's package on GitHub (darwin-x64 cross-compiled on Apple silicon, so its tests do not run; the rest natively) on each push and pull request, and on a `v<version>` tag publishes them to the Marketplace and Open VSX and as a GitHub release (see README.md, "Release it"). Clippy and the client's tests run on linux-x64 only.
- `sh scripts/package.sh` builds a VSIX for this machine's platform, or for `VSCODE_TARGET` (with `CARGO_BUILD_TARGET` for the Rust target), at the repository root: a release build of the server (with link-time optimization, one codegen unit and symbols stripped) in `server/`, and the client bundled by esbuild into `out/extension.js`. Install it with **Extensions: Install from VSIX…**.
- The client finds the server through, in order: the `askama.server.path` setting (from user settings only), then `server/askama-lsp` in the extension (a packaged extension) and `target/debug/askama-lsp` (this checkout), the latter first when run from the checkout (F5).
- **Askama: Restart Language Server** restarts the server after a rebuild without reloading the window.
