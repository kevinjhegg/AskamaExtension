//! End-to-end tests of the structural requests: document symbols, folding ranges and selection ranges.

mod common;

use common::{Crate, Session};
use serde_json::{Value, json};

const TEMPLATE: &str = "\
{% extends \"base.html\" %}
{% block content %}
{# A comment
   on two lines. #}
<table>
  {% for item in items %}
  <tr><td>{{ item.name }}</td></tr>
  {% endfor %}
</table>
{% endblock %}
";

fn open_template() -> (Crate, Session, String) {
    let krate = Crate::new();
    let uri = krate.uri("templates/page.html");
    let mut server = Session::start();
    server.open(&uri, TEMPLATE);
    server.diagnostics_for(&uri);
    (krate, server, uri)
}

fn names(symbols: &Value) -> Vec<String> {
    symbols
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|symbol| {
            let name = symbol["name"].as_str().unwrap().to_owned();
            std::iter::once(name).chain(names(&symbol["children"]).into_iter().map(|child| format!("  {child}")))
        })
        .collect()
}

#[test]
fn outlines_the_template() {
    let (_krate, mut server, uri) = open_template();
    let symbols = server.request("textDocument/documentSymbol", json!({ "textDocument": { "uri": uri } }));
    assert_eq!(names(&symbols), ["extends \"base.html\"", "block content", "  for item in items"]);
    let block = &symbols[1];
    assert_eq!(block["range"], json!({ "start": { "line": 1, "character": 0 }, "end": { "line": 9, "character": 14 } }));
    assert_eq!(block["selectionRange"]["end"], json!({ "line": 1, "character": 19 }));
}

#[test]
fn folds_regions_and_comments() {
    let (_krate, mut server, uri) = open_template();
    let folds = server.request("textDocument/foldingRange", json!({ "textDocument": { "uri": uri } }));
    let folds: Vec<(u64, u64, &str)> = folds
        .as_array()
        .unwrap()
        .iter()
        .map(|fold| (fold["startLine"].as_u64().unwrap(), fold["endLine"].as_u64().unwrap(), fold["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(folds, [(1, 8, "region"), (5, 6, "region"), (2, 3, "comment")]);
}

#[test]
fn expands_selection_from_a_word() {
    let (_krate, mut server, uri) = open_template();
    let position = json!({ "line": 6, "character": 19 });
    let result = server.request("textDocument/selectionRange", json!({ "textDocument": { "uri": uri }, "positions": [position] }));
    let mut ranges = Vec::new();
    let mut selection = &result[0];
    while selection.is_object() {
        let range = &selection["range"];
        ranges.push((range["start"]["line"].as_u64().unwrap(), range["start"]["character"].as_u64().unwrap(), range["end"]["line"].as_u64().unwrap(), range["end"]["character"].as_u64().unwrap()));
        selection = &selection["parent"];
    }
    assert_eq!(
        ranges,
        [
            (6, 18, 6, 22),  // name
            (6, 13, 6, 22),  // item.name
            (6, 10, 6, 25),  // {{ item.name }}
            (5, 25, 7, 2),   // the loop's body
            (5, 2, 7, 14),   // the loop
            (1, 19, 9, 0),   // the block's body
            (1, 0, 9, 14),   // the block
            (0, 0, 10, 0),   // the template
        ]
    );
}
