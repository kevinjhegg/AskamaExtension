//! End-to-end tests of navigation between templates: links, definitions, references,
//! implementations, workspace symbols and missing-template warnings.

mod common;

use common::{Crate, Session};
use serde_json::{Value, json};

const BASE: &str = "<title>{% block title %}{% endblock %}</title>\n{% block content %}{% endblock %}\n";
const PAGE: &str = "\
{% extends \"base.html\" %}
{% import \"macros.html\" as m %}
{% block content %}
  {% call m::cell(1) %}{% endcall %}
  {% include \"missing.html\" %}
{% endblock %}
";
const MACROS: &str = "{% macro cell(x) %}<td>{{ x }}</td>{% endmacro %}\n";

/// A crate with the three templates written, a server with `page.html` open, and its URI.
fn site() -> (Crate, Session, String) {
    let krate = Crate::new();
    krate.write("templates/base.html", BASE);
    krate.write("templates/page.html", PAGE);
    krate.write("templates/macros.html", MACROS);
    let uri = krate.uri("templates/page.html");
    let mut server = Session::start();
    server.open(&uri, PAGE);
    (krate, server, uri)
}

fn at(uri: &str, line: u32, character: u32) -> Value {
    json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } })
}

fn range(start: (u32, u32), end: (u32, u32)) -> Value {
    json!({ "start": { "line": start.0, "character": start.1 }, "end": { "line": end.0, "character": end.1 } })
}

#[test]
fn warns_about_a_missing_template() {
    let (_krate, mut server, uri) = site();
    let diagnostics = server.diagnostics_for(&uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["severity"], 2);
    assert_eq!(diagnostics[0]["range"], range((4, 14), (4, 26)));
    assert!(diagnostics[0]["message"].as_str().unwrap().starts_with("template \"missing.html\" not found"));
}

#[test]
fn links_template_paths_that_exist() {
    let (krate, mut server, uri) = site();
    let links = server.request("textDocument/documentLink", json!({ "textDocument": { "uri": uri } }));
    let links: Vec<(Value, Value)> = links.as_array().unwrap().iter().map(|link| (link["range"].clone(), link["target"].clone())).collect();
    assert_eq!(
        links,
        [
            (range((0, 12), (0, 21)), json!(krate.uri("templates/base.html"))),
            (range((1, 11), (1, 22)), json!(krate.uri("templates/macros.html"))),
        ]
    );
}

#[test]
fn goes_to_definitions() {
    let (krate, mut server, uri) = site();
    let to_base = server.request("textDocument/definition", at(&uri, 0, 14));
    assert_eq!(to_base, json!({ "uri": krate.uri("templates/base.html"), "range": range((0, 0), (0, 0)) }));

    let to_macro = server.request("textDocument/definition", at(&uri, 3, 14));
    assert_eq!(to_macro, json!({ "uri": krate.uri("templates/macros.html"), "range": range((0, 9), (0, 13)) }));

    let to_block = server.request("textDocument/definition", at(&uri, 2, 11));
    assert_eq!(to_block, json!({ "uri": krate.uri("templates/base.html"), "range": range((1, 9), (1, 16)) }));

    let to_scope = server.request("textDocument/definition", at(&uri, 3, 10));
    assert_eq!(to_scope, json!({ "uri": uri, "range": range((1, 27), (1, 28)) }));
}

#[test]
fn finds_the_blocks_that_override_one() {
    let (krate, mut server, _) = site();
    let base = krate.uri("templates/base.html");
    server.open(&base, BASE);
    let page = json!({ "uri": krate.uri("templates/page.html"), "range": range((2, 9), (2, 16)) });

    let mut params = at(&base, 1, 11);
    params["context"] = json!({ "includeDeclaration": true });
    let references = server.request("textDocument/references", params);
    assert_eq!(references, json!([{ "uri": base, "range": range((1, 9), (1, 16)) }, page]));

    let implementations = server.request("textDocument/implementation", at(&base, 1, 11));
    assert_eq!(implementations, json!([page]));

    // `title` is not overridden anywhere.
    assert_eq!(server.request("textDocument/implementation", at(&base, 0, 18)), json!([]));
}

#[test]
fn finds_blocks_and_macros_in_the_workspace() {
    let (krate, mut server, _) = site();
    let symbols = server.request("workspace/symbol", json!({ "query": "cel" }));
    assert_eq!(
        symbols,
        json!([{
            "name": "cell",
            "kind": 12,
            "containerName": "macros.html",
            "location": { "uri": krate.uri("templates/macros.html"), "range": range((0, 9), (0, 13)) }
        }])
    );
    let all = server.request("workspace/symbol", json!({ "query": "" }));
    assert_eq!(all.as_array().unwrap().len(), 4, "{all}");
}
