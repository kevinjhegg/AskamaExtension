//! End-to-end tests: start the `askama-lsp` binary, talk LSP to it over stdin and stdout as VS Code does, and check the diagnostics it
//! publishes.

mod common;

use common::{Crate, Session};
use serde_json::{Value, json};

#[test]
fn reports_a_syntax_error_and_clears_it_when_fixed() {
    let krate = Crate::new();
    let uri = krate.uri("templates/order.html");
    let mut server = Session::start();

    server.open(&uri, "{% for x in xs %}\n  {% call m::cell(x) %}\n{% endfor %}\n");
    let diagnostics = server.diagnostics_for(&uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic["message"], "expected `endcall` to terminate `call` node, found `endfor`");
    assert_eq!(diagnostic["source"], "askama");
    assert_eq!(diagnostic["severity"], 1);
    assert_eq!(diagnostic["range"], json!({ "start": { "line": 2, "character": 3 }, "end": { "line": 2, "character": 9 } }));

    server.change(&uri, 2, "{% for x in xs %}\n  {% call m::cell(x) %}{% endcall %}\n{% endfor %}\n");
    assert_eq!(server.diagnostics_for(&uri), Vec::<Value>::new());
}

#[test]
fn uses_the_crate_syntax_and_reports_a_broken_config() {
    let krate = Crate::new();
    krate.write("askama.toml", "[general]\ndefault_syntax = \"square\"\n\n[[syntax]]\nname = \"square\"\nexpr_start = \"[[\"\nexpr_end = \"]]\"\n");
    let uri = krate.uri("templates/page.html");
    let mut server = Session::start();

    // Valid with the crate's syntax, though `{{` is not closed.
    server.open(&uri, "<p>[[ name ]] {{ </p>");
    assert_eq!(server.diagnostics_for(&uri), Vec::<Value>::new());

    // A second server reads a broken configuration: the error lands on askama.toml.
    krate.write("askama.toml", "[general]\ndefault_syntax = \"missing\"\n");
    let mut server = Session::start();
    server.open(&uri, "<p>{{ name }}</p>");
    let diagnostics = server.diagnostics_for(&krate.uri("askama.toml"));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["message"], "default syntax \"missing\" not found");
    assert_eq!(diagnostics[0]["range"]["start"], json!({ "line": 1, "character": 17 }));
    // The template is still checked, with Askama's defaults.
    assert_eq!(server.diagnostics_for(&uri), Vec::<Value>::new());
}
