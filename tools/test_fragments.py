#!/usr/bin/env python3
"""Writes every string literal in a crate's test code, cut at format placeholders, as a JSON list:
the texts askama_layout.py --protect keeps whole. Reads tests/**/*.rs and each src file from its
first #[cfg(test)].

Usage: test_fragments.py CRATE_DIR OUT.json
"""
import json, pathlib, re, sys
root = pathlib.Path(sys.argv[1])
sources = [p.read_text() for p in sorted((root / "tests").rglob("*.rs"))]
for p in sorted((root / "src").rglob("*.rs")):
    text = p.read_text()
    at = text.find("#[cfg(test)]")
    if at >= 0:
        sources.append(text[at:])
literal = re.compile(r"""'(?:\\.|[^'\\\n])'|(?<!\w)b?r(#*)"(.*?)"\1|(?<!\w)b?"((?:\\.|[^"\\])*)\"""", re.S)
def unescape(body):
    return re.sub(r'\\(.)', lambda m: {"n": "\n", "t": "\t", "\\": "\\", '"': '"', "'": "'", "0": "\0"}.get(m.group(1), m.group(0)), body)
pieces = set()
for text in sources:
    text = re.sub(r"(?m)^\s*//[^\n]*", "", text)
    for m in literal.finditer(text):
        if m.group(2) is None and m.group(3) is None:
            continue  # a char literal
        body = m.group(2) if m.group(2) is not None else unescape(m.group(3))
        for piece in re.split(r"\{[^{}]*\}", body):
            piece = piece.replace("{{", "{").replace("}}", "}")
            if len(piece) >= 3 and re.search(r"\S.*\S", piece, re.S):
                pieces.add(piece)
json.dump(sorted(pieces), open(sys.argv[2], "w"))
print(len(pieces), "fragments")
