//! End-to-end tests of what the server knows from template structs: variable completion, hover and definition in templates, and the
//! template lens and link in Rust files.

mod common;

use common::{Crate, Session};
use serde_json::{Value, json};

const MAIN: &str = "\
use askama::Template;

#[derive(Template)]
#[template(path = \"page.html\")]
pub struct Page {
    /// The page title.
    pub title: String,
    pub count: u32,
}

impl Page {
    pub fn is_empty(&self) -> bool { self.count == 0 }
}
";

const BASE: &str = "<title>{% block title %}{% endblock %}</title>{% block content %}{% endblock %}\n";
const PAGE: &str = "{% extends \"base.html\" %}{% block content %}{{ title }} {{  }}{% endblock %}\n";

fn site() -> (Crate, Session) {
    let krate = Crate::new();
    krate.write("src/main.rs", MAIN);
    krate.write("templates/base.html", BASE);
    krate.write("templates/page.html", PAGE);
    (krate, Session::start())
}

fn at(uri: &str, line: u32, character: u32) -> Value {
    json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } })
}

fn members(items: &Value) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["label"].as_str().unwrap().to_owned(), item["detail"].as_str().unwrap().to_owned()))
        .collect();
    found.sort();
    found
}

#[test]
fn completes_the_struct_fields_in_its_template_and_the_one_it_extends() {
    let (krate, mut server) = site();
    let page = krate.uri("templates/page.html");
    server.open(&page, PAGE);
    let expected = [
        ("count".to_owned(), "u32".to_owned()),
        ("is_empty".to_owned(), "fn is_empty(&self) -> bool".to_owned()),
        ("title".to_owned(), "String".to_owned()),
    ];
    assert_eq!(members(&server.request("textDocument/completion", at(&page, 0, 59))), expected);

    let base = krate.uri("templates/base.html");
    server.open(&base, "{{  }}");
    assert_eq!(members(&server.request("textDocument/completion", at(&base, 0, 3))), expected);
}

#[test]
fn opens_the_list_after_a_space_where_a_name_belongs() {
    let (krate, mut server) = site();
    let page = krate.uri("templates/page.html");
    server.open(&page, PAGE);
    let mut params = at(&page, 0, 59);
    params["context"] = json!({ "triggerKind": 2, "triggerCharacter": " " });
    assert_eq!(members(&server.request("textDocument/completion", params)).len(), 3);
    // After a complete name, a space is just a space.
    let mut params = at(&page, 0, 53);
    params["context"] = json!({ "triggerKind": 2, "triggerCharacter": " " });
    assert_eq!(server.request("textDocument/completion", params), Value::Null);
}

#[test]
fn describes_and_finds_a_field() {
    let (krate, mut server) = site();
    let page = krate.uri("templates/page.html");
    server.open(&page, PAGE);
    let hover = server.request("textDocument/hover", at(&page, 0, 50));
    assert_eq!(hover["contents"]["value"], "```rust\ntitle: String\n```\n\nThe page title.\n\nField of `Page`.");

    let found = server.request("textDocument/definition", at(&page, 0, 50));
    assert_eq!(found["uri"], krate.uri("src/main.rs"));
    assert_eq!(found["range"], json!({ "start": { "line": 6, "character": 8 }, "end": { "line": 6, "character": 13 } }));
}

#[test]
fn links_a_struct_to_its_template() {
    let (krate, mut server) = site();
    let main = krate.uri("src/main.rs");
    server.open_as(&main, "rust", MAIN);
    let lenses = server.request("textDocument/codeLens", json!({ "textDocument": { "uri": main } }));
    assert_eq!(
        lenses,
        json!([{
            "range": { "start": { "line": 4, "character": 11 }, "end": { "line": 4, "character": 15 } },
            "command": { "title": "Open template page.html", "command": "askama.openTemplate", "arguments": [krate.uri("templates/page.html")] }
        }])
    );

    let found = server.request("textDocument/definition", at(&main, 3, 21));
    assert_eq!(found["uri"], krate.uri("templates/page.html"));

    // A Rust file is not a template: no diagnostics, outline or completion from it.
    assert_eq!(server.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": main } })), Value::Null);
}
