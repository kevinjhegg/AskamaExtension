# Askama IntelliSense

Editing support for [Askama](https://github.com/askama-rs/askama) templates in Rust projects: highlighting, errors as you type, navigation between templates, and completion that knows the Rust struct behind each template and the types of its fields.

Templates stay HTML, so VS Code's own HTML support and any extension that works on HTML (Emmet, formatters, Tailwind or htmx helpers) keep working in them.

## Features

**Highlighting.** Askama's tags, keywords, filters, variables and comments inside HTML, including inside attribute values, and htmx attributes (`hx-*`) in their own color. Snippets for the block tags (`for`, `if`, `block`, `macro`, `match` and more), also as `%for`, `%if` and so on.

**Errors as you type.** Syntax errors from Askama's own parser, where the compiler would report them. Mistakes in `askama.toml`. Warnings for a template path that does not exist, a filter that is neither built in nor the crate's own, and a variable that is neither bound in the template nor a field of its struct. While the private rust-analyzer runs (see Requirements), unknown fields and methods deep in an expression (`order.customer.mail`) are errors too.

**Outline, folding and selection.** Blocks, loops and conditions in the Outline and breadcrumbs, a fold for each branch, and Expand Selection from a name out to its tag and block.

**Navigation.** Cmd+click (Ctrl+click) from `extends`, `include` and `import` paths to the template, from a block to the block it overrides, from a macro call to the macro, from a filter to its Rust function, and from a variable to its field in Rust or to where the template binds it. Find All References and Go to Implementations on a block. Go to Symbol in Workspace for blocks and macros. In Rust, an **Open template** link above each template struct.

**Completion and hover.** The struct's fields and methods, the template's own names (`for` variables, `loop`, `let`, macro arguments, `match` and `if let` patterns, a call block's parameters), filters with their signatures, tag keywords with the closing tag first, and, through rust-analyzer, the members of any value: `{{ order.customer.` lists `Customer`'s fields with their types, and calls and indexing work too (`order.items.first().`, `order.items[i].`). Hover shows types and documentation. Rename Symbol renames a name the template binds across its uses.

**Editing.** In templates only: Cmd+/ (Ctrl+/) comments lines with `{# … #}`, typing `{%`, `{{` or `{#` adds its closer, and **Askama: Surround with Tag** wraps a selection in a block tag pair.

## Requirements

- A Rust crate using Askama 0.16, with templates in a `templates` directory or where its `askama.toml` says.
- For completion and errors that need types: rust-analyzer. The extension uses the one the rust-analyzer extension ships, or `askama.rustAnalyzer.path`. It runs its own copy for each crate, started the first time a type is needed, not when a template is opened; the first answer waits while it loads the crate, and a progress notice says when it is ready. It takes as much memory as rust-analyzer takes for the crate's whole Cargo workspace, which can be several gigabytes for a large one, and it stops after ten minutes without a question, freeing that memory; the next completion or hover that needs a type starts it again. Like your own rust-analyzer, it runs the crate's build scripts and procedural macros, unless `rust-analyzer.cargo.buildScripts.enable` or `rust-analyzer.procMacro.enable` turns that off.
- A trusted workspace. In Restricted Mode the extension stays off, since what it starts runs the crate's code.

## Settings

| Setting | Default | |
|---|---|---|
| `askama.templateGlobs` | `["**/templates/**"]` | Which files are templates. |
| `askama.typeCompletion` | `true` | Ask rust-analyzer about types. Turn off to save its memory. |
| `askama.rustAnalyzer.path` | empty | The rust-analyzer to use, if not the rust-analyzer extension's. User settings only. |
| `askama.server.path` | empty | A language server other than the bundled one. User settings only. |
| `askama.trace.server` | `off` | Log messages between VS Code and the server. |

Run **Askama: Restart Language Server** after changing a setting. The two path settings name programs to run, so the extension reads them only from your user settings: a repository's `.vscode/settings.json` cannot choose them.

## Known limits

- Templates are checked with the crate's default syntax. A struct's own `syntax = "..."` and `config = "..."` are not followed, and highlighting knows only the default delimiters.
- Inline templates (`#[template(source = "...")]`) are not read.
- Changes to Rust files count once saved.
- Members of a macro argument are known only when the macro is called from one place.
- A call block's parameters (`user` in `{% call(user) list(users) %}`) are known by name only: their members are not completed.
- Type errors show only while the private rust-analyzer runs: after a completion or hover that needs a type has started it, and until it has been idle ten minutes.
- Not type checked: an expression with a filter inside brackets (`f(x|upper)`), `is defined`, `loop`, and expressions inside macros or using a call block's parameters. A filter at the end is fine: `order.total|money` checks `order.total`.
