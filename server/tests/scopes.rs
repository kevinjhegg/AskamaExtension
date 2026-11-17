//! End-to-end tests of the names a template binds: completion, `loop` members, hover,
//! definition, rename, references, and the unknown-variable warning.

mod common;

use common::{Crate, Session};
use serde_json::{Value, json};

const MAIN: &str = "\
#[derive(askama::Template)]
#[template(path = \"page.html\")]
pub struct Page {
    pub title: String,
    pub items: Vec<String>,
}
";

const PAGE: &str = "\
{% for item in items %}
  {{ item }} {{ loop.index }} {{ itme }}
{% endfor %}
{% let total = items.len() %}{{ total }}
";

fn site() -> (Crate, Session, String) {
    let krate = Crate::new();
    krate.write("src/main.rs", MAIN);
    krate.write("templates/page.html", PAGE);
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

fn labels_in_order(items: &Value) -> Vec<String> {
    let mut items: Vec<(String, String)> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["sortText"].as_str().unwrap_or("").to_owned(), item["label"].as_str().unwrap().to_owned()))
        .collect();
    items.sort();
    items.into_iter().map(|(_, label)| label).collect()
}

#[test]
fn warns_about_an_unknown_variable_only() {
    let (_krate, mut server, uri) = site();
    let diagnostics = server.diagnostics_for(&uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["range"], range((1, 33), (1, 37)));
    assert!(diagnostics[0]["message"].as_str().unwrap().starts_with("unknown variable `itme`"));
}

#[test]
fn completes_bindings_before_fields() {
    let (_krate, mut server, uri) = site();
    let items = server.request("textDocument/completion", at(&uri, 1, 5));
    assert_eq!(labels_in_order(&items), ["item", "loop", "items", "title"]);
    let item = items.as_array().unwrap().iter().find(|item| item["label"] == "item").unwrap();
    assert_eq!(item["detail"], "loop variable over `items`");
}

#[test]
fn completes_loop_members() {
    let (_krate, mut server, uri) = site();
    let mut params = at(&uri, 1, 21);
    params["context"] = json!({ "triggerKind": 2, "triggerCharacter": "." });
    let items = server.request("textDocument/completion", params);
    let mut labels: Vec<&str> = items.as_array().unwrap().iter().map(|item| item["label"].as_str().unwrap()).collect();
    labels.sort_unstable();
    assert_eq!(labels, ["cycle", "first", "index", "index0", "last"]);
    // After any other name's `.`, nothing yet: its members need its type.
    let mut params = at(&uri, 3, 21);
    params["context"] = json!({ "triggerKind": 2, "triggerCharacter": "." });
    assert_eq!(server.request("textDocument/completion", params), Value::Null);
}

#[test]
fn describes_and_finds_a_binding() {
    let (_krate, mut server, uri) = site();
    let hover = server.request("textDocument/hover", at(&uri, 1, 6));
    assert_eq!(hover["contents"]["value"], "**`item`**: loop variable over `items`");
    let found = server.request("textDocument/definition", at(&uri, 1, 6));
    assert_eq!(found, json!({ "uri": uri, "range": range((0, 7), (0, 11)) }));
    let index = server.request("textDocument/hover", at(&uri, 1, 22));
    assert!(index["contents"]["value"].as_str().unwrap().starts_with("**`loop.index`**"));
}

#[test]
fn renames_a_binding_and_its_uses() {
    let (_krate, mut server, uri) = site();
    let prepared = server.request("textDocument/prepareRename", at(&uri, 1, 6));
    assert_eq!(prepared, range((1, 5), (1, 9)));

    let mut params = at(&uri, 1, 6);
    params["newName"] = json!("line");
    let edit = server.request("textDocument/rename", params);
    assert_eq!(
        edit["changes"][uri.as_str()],
        json!([{ "range": range((0, 7), (0, 11)), "newText": "line" }, { "range": range((1, 5), (1, 9)), "newText": "line" }])
    );

    let mut references = at(&uri, 0, 8);
    references["context"] = json!({ "includeDeclaration": false });
    assert_eq!(server.request("textDocument/references", references), json!([{ "uri": uri, "range": range((1, 5), (1, 9)) }]));

    // A field is not the template's to rename.
    assert_eq!(server.request("textDocument/prepareRename", at(&uri, 0, 16)), Value::Null);
}

#[test]
fn reports_only_the_syntax_error_while_a_tag_is_unclosed() {
    let (_krate, mut server, uri) = site();
    server.diagnostics_for(&uri);
    // Mid-typing: the expression is not closed yet, so the rest of the file reads as part of it.
    server.change(&uri, 2, "{% for item in items %}\n  {{ item.sku\n  <tr><td>x</td></tr>\n{% endfor %}\n");
    let diagnostics = server.diagnostics_for(&uri);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["severity"], 1);
}
