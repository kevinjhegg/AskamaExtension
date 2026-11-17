// Tests of the editing conveniences, run by `npm test` with Node's own test runner.

import { strict as assert } from "node:assert";
import { test } from "node:test";
import { autoClose, escapeSnippet, surround, toggleComment } from "./editing.js";

test("comments out lines at their indentation", () => {
  assert.deepEqual(toggleComment(["  <p>", "    {{ x }}", "  </p>"]), ["  {# <p>", "    {{ x }}", "  </p> #}"]);
  assert.deepEqual(toggleComment(["<p>x</p>  "]), ["{# <p>x</p> #}  "]);
});

test("uncomments what is commented", () => {
  assert.deepEqual(toggleComment(["  {# <p>", "  </p> #}"]), ["  <p>", "  </p>"]);
  assert.deepEqual(toggleComment(["{#<p>x</p>#}"]), ["<p>x</p>"]);
});

test("toggling twice gives the lines back", () => {
  const lines = ["  <td>{{ item.sku }}</td>", "  <td>{{ item.quantity }}</td>"];
  assert.deepEqual(toggleComment(toggleComment(lines)), lines);
});

test("closes each opener, replacing the editor's own closing braces", () => {
  // After `{%` with the editor's `}` from the first `{`: `{%|}` becomes `{% | %}`.
  assert.deepEqual(autoClose("<p>{%", "}</p>"), { remove: 1, insert: "  %}", cursorAfter: 1 });
  // After `{{` with `}}`: `{{|}}` becomes `{{ | }}`.
  assert.deepEqual(autoClose("{{", "}}"), { remove: 2, insert: "  }}", cursorAfter: 1 });
  assert.deepEqual(autoClose("{#", ""), { remove: 0, insert: "  #}", cursorAfter: 1 });
});

test("leaves alone what is already closed or not an opener", () => {
  assert.equal(autoClose("{%", " %}"), undefined);
  assert.equal(autoClose("{{", " }}"), undefined);
  assert.equal(autoClose("{{{", "}}}"), undefined);
  assert.equal(autoClose("a{", "}"), undefined);
});

test("surrounds text within a line inline", () => {
  assert.equal(surround("if", "<b>x</b>", false), "{% if ${1:condition} %}<b>x</b>{% endif %}");
  assert.equal(surround("raw", "{{ x }}", false), "{% raw %}{{ x \\}\\}{% endraw %}");
});

test("surrounds whole lines with tags on their own lines", () => {
  assert.equal(
    surround("for", "  <li>a</li>\n  <li>b</li>\n", true),
    "  {% for ${1:item in items} %}\n  <li>a</li>\n  <li>b</li>\n  {% endfor %}\n",
  );
  assert.equal(surround("match", "x\n", true), "{% match ${1:value} %}\n{% when ${2:pattern} %}\nx\n{% endmatch %}\n");
});

test("escapes snippet syntax", () => {
  assert.equal(escapeSnippet("$a } \\"), "\\$a \\} \\\\");
});
