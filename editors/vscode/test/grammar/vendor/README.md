# Vendored grammars for the grammar tests

The tests tokenize templates the way VS Code does, so they need the grammars Askama's injection sits on.

- `html.tmLanguage.json` and `html-derivative.tmLanguage.json` are copied unchanged from VS Code 1.146's built-in `html` extension (`Contents/Resources/app/extensions/html/syntaxes`). They are converted from [textmate/html.tmbundle](https://github.com/textmate/html.tmbundle). Refresh them when VS Code's HTML grammar changes.
- `source.js.stub.json` and `source.css.stub.json` stand in for the JavaScript and CSS grammars the HTML grammar includes. Without them a rule that includes `source.js` is dropped during the tests (but not in VS Code).

None of these files ship in the extension.
