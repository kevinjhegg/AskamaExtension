#!/usr/bin/env python3
"""Lays out long lines of Askama HTML templates without changing what the browser draws.

A line longer than LIMIT is split, aiming at WIDTH, only where whitespace cannot show:

- next to an Askama tag, whose trim marker (`{%-`, `-%}`, `{{-`, ...) is added on that side so the new whitespace never reaches the output;
- between the children of a flex, grid, table, select or SVG element;
- in a block container, between two siblings when either is a block-level box, and at the container's start and end (whitespace at the start
  or end of a line is collapsed);
- inside a start tag, between attributes.

What an element is comes from its tag and from the stylesheet passed with --css. A rule that applies only in some context (a media query, a
state, another class, a parent) counts as one more thing the element may be, and a class outranks a tag. A class an expression completes
(`ks-alert-{{ variant }}`) may be any class in the stylesheet that fits; a class that is all expression may be anything. A boundary is split
only if it is safe for everything the element and its neighbors may be. Nothing inside text, attribute values, comments or elements whose
whitespace is kept (`pre`, `textarea`, `white-space: pre*`) is touched. Existing line breaks stay where they are, except between the
attributes of a start tag, which is laid out again with the rest of its line. A trim marker never goes where the neighbor has whitespace of
its own, since the marker would remove it.

Long Askama tags that start a line have their arguments wrapped at commas, which Askama ignores.

The page is drawn the same, but its HTML text gains line breaks. --protect takes a JSON list of texts (test_fragments.py writes one from a
crate's tests) that no new break may split, following Askama's branches and loops and letting each `{{ }}` stand for any text.

Usage: askama_layout.py --css app.css [--protect fragments.json] [--check] FILE...
Run it twice: a line the first pass shortens may become one the second pass can wrap.
"""

import argparse
import json
import re
import sys
from dataclasses import dataclass, field

LIMIT = 140
WIDTH = 120
STEP = 4

VOID = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"}
RAW = {"script", "style", "textarea", "pre", "title"}
BLOCK_TAGS = {
    "address", "article", "aside", "blockquote", "body", "dd", "details", "dialog", "div", "dl", "dt", "fieldset",
    "figcaption", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hgroup", "hr", "html",
    "legend", "li", "main", "menu", "nav", "ol", "p", "pre", "search", "section", "summary", "ul",
}
TABLE_TAGS = {"table", "thead", "tbody", "tfoot", "tr", "colgroup"}
CELL_TAGS = {"td", "th", "caption"}
IGNORED_WHITESPACE_TAGS = {"select", "optgroup", "datalist", "svg", "g", "defs", "head", "html"}
ASKAMA_OPENERS = {"if", "for", "match", "block", "macro", "call", "filter", "raw"}
ASKAMA_CLOSERS = {"endif", "endfor", "endmatch", "endblock", "endmacro", "endcall", "endfilter", "endraw"}
ASKAMA_MIDDLES = {"else", "elif", "when"}


# --- The stylesheet ---------------------------------------------------------------------------

@dataclass
class Rule:
    """One rule's value for one property, and when it applies: to an element that also has the classes `requires`, whose parent has the
    classes `parent` (a `>` combinator), and always when `anywhere` (a media query, a state, an ancestor further up)."""
    prop: str
    value: str
    requires: frozenset = frozenset()
    parent: frozenset = None
    anywhere: bool = False


@dataclass
class Style:
    """What the stylesheet says about one class or tag."""
    rules: list = field(default_factory=list)
    pseudo_content: bool = False


def parse_css(text):
    text = re.sub(r"/\*.*?\*/", "", text, flags=re.S)
    classes, tags = {}, {}

    def rules(body, conditional):
        at = 0
        while at < len(body):
            brace = body.find("{", at)
            if brace < 0:
                return
            prelude = body[at:brace].strip()
            depth, end = 1, brace + 1
            while end < len(body) and depth:
                depth += {"{": 1, "}": -1}.get(body[end], 0)
                end += 1
            inner = body[brace + 1:end - 1]
            at = end
            if prelude.startswith("@"):
                if prelude.startswith(("@media", "@supports", "@layer", "@container")):
                    rules(inner, True)
                continue
            declarations = {}
            for part in inner.split(";"):
                if ":" in part:
                    name, value = part.split(":", 1)
                    declarations[name.strip().lower()] = value.replace("!important", "").strip().lower()
            for selector in split_top(prelude, ","):
                apply(selector.strip(), declarations, conditional)

    def class_names(compound):
        return [re.sub(r"\\(.)", r"\1", c) for c in re.findall(r"\.((?:\\.|[\w-])+)", compound)]

    def apply(selector, declarations, conditional):
        parts = split_combinators(selector)
        compound = parts[-1]
        if "::" in compound or re.search(r":(before|after)\b", compound):
            if "content" in declarations:
                for name in class_names(compound):
                    classes.setdefault(name, Style()).pseudo_content = True
                tag = re.match(r"^[a-zA-Z][\w-]*", compound)
                if tag:
                    tags.setdefault(tag.group(0).lower(), Style()).pseudo_content = True
            return
        names = class_names(compound)
        tag = re.match(r"^[a-zA-Z][\w-]*", compound)
        anywhere = conditional or bool(re.search(r"[:\[#]", compound))
        parent = None
        if len(parts) > 1:
            child = re.search(r">\s*" + re.escape(compound) + r"\s*$", selector)
            previous = parts[-2]
            if child and not re.search(r"[:\[#]", previous) and class_names(previous):
                parent = frozenset(class_names(previous))
            else:
                anywhere = True
        for prop in ("display", "position", "white-space", "float"):
            if prop not in declarations:
                continue
            value = declarations[prop].split()[0] if declarations[prop] else ""
            if names:
                # Kept on the first class, requiring the others (and ignoring the tag, which only narrows it further).
                rule = Rule(prop, value, frozenset(names[1:]), parent, anywhere)
                classes.setdefault(names[0], Style()).rules.append(rule)
            elif tag:
                tags.setdefault(tag.group(0).lower(), Style()).rules.append(Rule(prop, value, frozenset(), parent, anywhere))
            elif compound == "*" and len(parts) == 1:
                pass

    rules(text, False)
    return classes, tags


def split_top(text, separator):
    parts, depth, start = [], 0, 0
    for i, c in enumerate(text):
        depth += {"(": 1, "[": 1, ")": -1, "]": -1}.get(c, 0)
        if c == separator and depth == 0:
            parts.append(text[start:i])
            start = i + 1
    parts.append(text[start:])
    return parts


def split_combinators(selector):
    parts, depth, current, i = [], 0, "", 0
    while i < len(selector):
        c = selector[i]
        if c == "\\" and i + 1 < len(selector):
            current += selector[i:i + 2]
            i += 2
            continue
        depth += {"(": 1, "[": 1, ")": -1, "]": -1}.get(c, 0)
        if depth == 0 and c in " >+~":
            if current.strip():
                parts.append(current.strip())
            current = ""
        else:
            current += c
        i += 1
    if current.strip():
        parts.append(current.strip())
    return parts or [""]


# --- Tokens ---------------------------------------------------------------------------------

@dataclass
class Token:
    kind: str  # text, comment, askama_block, askama_expr, askama_comment, start, end, raw
    start: int
    end: int
    text: str
    name: str = ""  # tag name, or an Askama block tag's keyword
    attrs: list = field(default_factory=list)  # a start tag's attribute units, as (preceding whitespace, text)
    self_closing: bool = False
    tag_head: str = ""  # `<name`
    tag_tail: str = ""  # `>` or `/>`


def find_askama_end(src, i, close):
    j = i + 2
    while j < len(src):
        if src[j] in "\"'" and src[i:i + 2] != "{#":
            quote = src[j]
            j += 1
            while j < len(src) and src[j] != quote:
                j += 2 if src[j] == "\\" else 1
            j += 1
            continue
        if src.startswith(close, j):
            return j + 2
        j += 1
    return len(src)


def find_comment_end(src, i):
    depth, j = 1, i + 2
    while j < len(src) and depth:
        if src.startswith("{#", j):
            depth, j = depth + 1, j + 2
        elif src.startswith("#}", j):
            depth, j = depth - 1, j + 2
        else:
            j += 1
    return j


def askama_at(src, i):
    if src.startswith("{#", i):
        return "askama_comment", find_comment_end(src, i)
    if src.startswith("{%", i):
        return "askama_block", find_askama_end(src, i, "%}")
    if src.startswith("{{", i):
        return "askama_expr", find_askama_end(src, i, "}}")
    return None, i


def keyword(text):
    inner = text[2:-2].strip(" -+~")
    return re.match(r"[a-z_]*", inner).group(0)


def tokenize(src):
    tokens, i, n = [], 0, len(src)
    while i < n:
        kind, end = askama_at(src, i)
        if kind:
            token = Token(kind, i, end, src[i:end])
            if kind == "askama_block":
                token.name = keyword(token.text)
            tokens.append(token)
            i = end
            if kind == "askama_block" and token.name == "raw":
                close = re.compile(r"\{%[-+~]?\s*endraw\s*[-+~]?%\}").search(src, i)
                stop = close.start() if close else n
                tokens.append(Token("raw", i, stop, src[i:stop]))
                i = stop
            continue
        if src.startswith("<!--", i):
            end = src.find("-->", i)
            end = n if end < 0 else end + 3
            tokens.append(Token("comment", i, end, src[i:end]))
            i = end
            continue
        match = re.match(r"<(/?)([a-zA-Z][\w:-]*)", src[i:])
        if match:
            token, i = read_tag(src, i, match)
            tokens.append(token)
            if token.kind == "start" and token.name in RAW and not token.self_closing:
                close = re.compile(r"</" + token.name + r"\s*>", re.I).search(src, i)
                stop = close.start() if close else n
                if stop > i:
                    tokens.append(Token("raw", i, stop, src[i:stop]))
                i = stop
            continue
        j = i + 1
        while j < n and not src.startswith(("{%", "{{", "{#", "<!--"), j) and not re.match(r"</?[a-zA-Z]", src[j:j + 3]):
            j += 1
        tokens.append(Token("text", i, j, src[i:j]))
        i = j
    return tokens


def read_tag(src, i, match):
    closing, name = match.group(1) == "/", match.group(2).lower()
    j = i + len(match.group(0))
    head = src[i:j]
    units, whitespace = [], ""
    while j < len(src):
        c = src[j]
        if c.isspace():
            whitespace += c
            j += 1
            continue
        if c == ">" or src.startswith("/>", j):
            tail = ">" if c == ">" else "/>"
            end = j + len(tail)
            token = Token("end" if closing else "start", i, end, src[i:end], name=name, attrs=units,
                          self_closing=tail == "/>", tag_head=head, tag_tail=whitespace + tail)
            return token, end
        unit_start = j
        if src.startswith(("{%", "{{", "{#"), j):
            j = askama_group_end(src, j)
        else:
            while j < len(src) and not src[j].isspace() and src[j] not in ">=" and not src.startswith("/>", j) \
                    and not src.startswith(("{%", "{{"), j):
                j += 1
            if j < len(src) and src[j] == "=":
                j += 1
                if j < len(src) and src[j] in "\"'":
                    quote = src[j]
                    j += 1
                    while j < len(src) and src[j] != quote:
                        kind, end = askama_at(src, j)
                        j = end if kind else j + 1
                    j += 1
                else:
                    while j < len(src) and not src[j].isspace() and src[j] != ">":
                        kind, end = askama_at(src, j)
                        j = end if kind else j + 1
        # An Askama group right after an attribute, with no space, belongs to it: `value="{{ id }}"{% if checked %} checked{% endif %}`.
        while src.startswith(("{%", "{{"), j):
            j = askama_group_end(src, j)
        units.append((whitespace, src[unit_start:j], unit_start))
        whitespace = ""
    return Token("text", i, len(src), src[i:]), len(src)


def askama_group_end(src, j):
    """The end of an Askama tag, or of a whole `{% if %}...{% endif %}` (or `for`, `match`) group."""
    kind, end = askama_at(src, j)
    if kind != "askama_block" or keyword(src[j:end]) not in ("if", "for", "match"):
        return end
    depth, k = 1, end
    while k < len(src) and depth:
        kind, stop = askama_at(src, k)
        if kind == "askama_block":
            word = keyword(src[k:stop])
            depth += 1 if word in ("if", "for", "match") else -1 if word in ("endif", "endfor", "endmatch") else 0
            k = stop
        else:
            k += 1
    return k


# --- What each element is ---------------------------------------------------------------------

@dataclass
class Box:
    """What an element may be, as sets, since a rule may or may not apply."""
    outer: frozenset  # block, inline, none, unknown: how it sits among its siblings
    inner: frozenset  # flexgrid, block, table, ignore, inline, unknown: how its children are laid out
    keeps_whitespace: bool = False
    pseudo_content: bool = False
    contents: bool = False  # `display: contents`: its children belong to its parent


def category(display):
    if display in ("flex", "grid"):
        return "block", "flexgrid"
    if display in ("inline-flex", "inline-grid"):
        return "inline", "flexgrid"
    if display in ("block", "list-item", "flow-root"):
        return "block", "block"
    if display == "inline-block":
        return "inline", "block"
    if display in ("table", "inline-table", "table-row", "table-row-group", "table-header-group", "table-footer-group"):
        return ("block" if display != "inline-table" else "inline"), "table"
    if display in ("table-cell", "table-caption"):
        return "block", "block"
    if display == "inline":
        return "inline", "inline"
    if display == "ignore":
        return "inline", "ignore"
    if display == "none":
        return "none", "none"
    return "unknown", "unknown"


def class_words(token):
    """A start tag's classes: the names it always has, and a pattern for each class an expression completes (`ks-alert-{{ variant }}` as
    `ks-alert-.*`)."""
    attributes = " ".join(unit for _, unit, _ in token.attrs)
    match = re.search(r'class="([^"]*)"', attributes)
    text = re.sub(r"\{\{.*?\}\}", "\x00", match.group(1) if match else "")
    names, patterns = set(), []
    for word in re.split(r"\s+|\{%.*?%\}", text):
        if not word:
            continue
        if "\x00" in word:
            patterns.append(re.compile("".join("(?:.*)" if c == "\x00" else re.escape(c) for c in word)))
        else:
            names.add(word)
    return names, patterns


def class_list(token):
    attributes = " ".join(unit for _, unit, _ in token.attrs)
    match = re.search(r'class="([^"]*)"', attributes)
    text = match.group(1) if match else ""
    if "{{" in text:
        return None
    return {w for w in re.split(r"\s+|\{%.*?%\}", text) if w and not w.startswith("{")}


def element_box(token, classes, tags, parent_names):
    """`parent_names`: the parent element's classes, or None where the parent is not known."""
    name = token.name
    if name in BLOCK_TAGS:
        default = "block"
    elif name in TABLE_TAGS:
        default = "table-row-group" if name != "table" else "table"
    elif name in CELL_TAGS:
        default = "table-cell"
    elif name in IGNORED_WHITESPACE_TAGS:
        default = "ignore"
    else:
        default = "inline"
    unknown = Box(frozenset({"unknown"}), frozenset({"unknown"}), name in RAW)
    attributes = " ".join(unit for _, unit, _ in token.attrs)
    if re.search(r"(^|\s)hidden(\s|$|=)", attributes):
        return Box(frozenset({"none"}), frozenset({"none"}))
    names, patterns = class_words(token)
    if any(pattern.pattern == "(?:.*)" for pattern in patterns):
        return unknown  # a class that is all expression may be anything
    # Classes an expression may complete (`ks-alert-{{ variant }}`): each one in the stylesheet that fits may apply, so its rules count as
    # possible.
    maybe = {k for k in classes if k not in names and any(p.fullmatch(k) for p in patterns)}
    styles = [classes[n] for n in sorted(names | maybe) if n in classes] + ([tags[name]] if name in tags else [])

    def applies(rule):
        if not rule.requires <= names | maybe:
            return False
        return rule.parent is None or parent_names is None or rule.parent <= parent_names

    class_styles = [classes[n] for n in sorted(names) if n in classes]
    maybe_styles = [classes[n] for n in sorted(maybe)]
    tag_styles = [tags[name]] if name in tags else []

    def possible(prop, base):
        """A class selector outranks a tag selector, so a tag's plain value counts only when no class sets one."""
        found = []
        for group in (class_styles, tag_styles):
            plain, conditional = set(), set()
            for style in group:
                for rule in style.rules:
                    if rule.prop == prop and applies(rule):
                        (conditional if rule.anywhere or rule.requires or rule.parent else plain).add(rule.value)
            found.append((plain, conditional))
        (class_plain, class_conditional), (tag_plain, tag_conditional) = found
        maybe_values = {rule.value for style in maybe_styles for rule in style.rules if rule.prop == prop and applies(rule)}
        return (class_plain or tag_plain or {base}) | class_conditional | tag_conditional | maybe_values

    pseudo = any(style.pseudo_content for style in styles)
    displays = possible("display", default)
    style_attr = re.search(r'style="([^"]*)"', attributes)
    if style_attr and "display" in style_attr.group(1):
        value = re.search(r"display\s*:\s*([\w-]+)", style_attr.group(1))
        if not value or "{" in style_attr.group(1):
            return unknown
        displays = {value.group(1)}
    white_space = possible("white-space", "normal")
    keeps = name in RAW or any(w.startswith("pre") or w == "break-spaces" or not w for w in white_space)
    if displays == {"contents"}:
        return Box(frozenset({"contents"}), frozenset({"contents"}), keeps, pseudo, True)
    if "contents" in displays:
        return Box(unknown.outer, unknown.inner, keeps)
    outer = {category(d)[0] for d in displays}
    inner = {category(d)[1] for d in displays}
    if possible("position", "static") & {"absolute", "fixed"} or possible("float", "none") - {"none"}:
        outer.add("unknown")
    return Box(frozenset(outer), frozenset(inner), keeps, pseudo)


# --- Where the template may break -------------------------------------------------------------

ASKAMA_KINDS = ("askama_block", "askama_expr", "askama_comment")


@dataclass
class Boundary:
    safe: bool  # whitespace here would not be drawn
    keeps: bool  # inside an element that keeps its whitespace


def analyze(tokens, classes, tags, extends):
    """For each boundary between two tokens (index i: between tokens[i] and tokens[i + 1]): whether whitespace there is invisible. A macro's
    or a call's body is laid out wherever it is used, so it starts with no known parent."""
    boxes, boundaries = {}, {}
    stack = []  # (box, start token index, classes), innermost last
    saved = []  # the stack outside each macro or call body
    in_content = False
    main = Box(frozenset({"block"}), frozenset({"block"}))

    def context():
        for box, _, _ in reversed(stack):
            if not box.contents:
                return box
        if extends and in_content and not saved:
            return main  # a page's content block is laid out in `<main>`
        return None

    for i, token in enumerate(tokens):
        if token.kind == "start":
            parent_names = stack[-1][2] if stack else None
            box = element_box(token, classes, tags, parent_names)
            parent = context()
            if parent and parent.inner == {"flexgrid"} and not box.contents:
                # A flex or grid item is block-level, its contents in a block box if inline.
                box = Box(frozenset({"block"}), frozenset("block" if v == "inline" else v for v in box.inner),
                          box.keeps_whitespace, box.pseudo_content)
            boxes[i] = box
            if not token.self_closing and token.name not in VOID:
                stack.append((box, i, class_list(token)))
        elif token.kind == "end":
            for depth in range(len(stack) - 1, -1, -1):
                if tokens[stack[depth][1]].name == token.name:
                    boxes[i] = stack[depth][0]
                    del stack[depth:]
                    break
        elif token.kind == "askama_block":
            if token.name in ("macro", "call"):
                saved.append(stack)
                stack = []
            elif token.name in ("endmacro", "endcall") and saved:
                stack = saved.pop()
            elif token.name == "block" and re.search(r"\bblock\s+content\b", token.text):
                in_content = True
            elif token.name == "endblock":
                in_content = False
        if i + 1 < len(tokens):
            keeps = any(box.keeps_whitespace for box, _, _ in stack)
            safe = not keeps and boundary_is_safe(i, tokens, boxes, context())
            boundaries[i] = Boundary(safe, keeps)
    return boundaries


def boundary_is_safe(i, tokens, boxes, parent):
    """Safe only if it is for every way the parent and the two siblings may be laid out."""
    left, right = tokens[i], tokens[i + 1]

    def outers(token, index):
        if token.kind in ("start", "end") and index in boxes:
            return {"inline" if o in ("none", "contents") else o for o in boxes[index].outer}
        return {"inline"}

    at_start = left.kind == "start" and not left.self_closing and left.name not in VOID
    at_end = right.kind == "end"
    lefts = {None} if at_start else outers(left, i)
    rights = {None} if at_end else outers(right, i + 1)
    inners = parent.inner if parent else {"unknown"}
    pseudo = parent.pseudo_content if parent else False
    return all(safe_for(p, a, b, pseudo) for p in inners for a in lefts for b in rights)


def safe_for(parent, left, right, pseudo):
    if parent in ("flexgrid", "table", "ignore", "none"):
        return True
    if parent == "block":
        if left is None or right is None:
            return not pseudo
        return left == "block" or right == "block"
    if parent == "unknown":
        return left == "block" and right == "block"
    return False


def askama_side(token, side):
    """Whether a trim marker can go on this side of an Askama tag (not where `+` or `~` is)."""
    if token.kind not in ASKAMA_KINDS:
        return False
    marker = token.text[2:3] if side == "before" else token.text[-3:-2]
    return marker not in ("+", "~")


def with_trim(text, side):
    if side == "before":
        return text if text[2:3] == "-" else text[:2] + "-" + text[2:]
    return text if text[-3:-2] == "-" else text[:-2] + "-" + text[-2:]


def wrap_arguments(text, column, continuation):
    """An Askama tag with its call arguments wrapped at commas, each line after the first at `continuation`. Askama reads whitespace inside
    a tag as nothing, so this changes no output. Only the commas of the outermost argument list count, never one inside a string."""
    commas, depth, quote, outer = [], 0, None, None
    i = 2
    while i < len(text) - 2:
        c = text[i]
        if quote:
            if c == "\\":
                i += 2
                continue
            if c == quote:
                quote = None
        elif c == '"':
            quote = c
        elif c in "([{":
            depth += 1
            if depth == 1 and outer is None:
                outer = i
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 1:
            commas.append(i)
        i += 1
    if not commas:
        return text
    pieces, start = [], 0
    for comma in commas:
        pieces.append(text[start:comma + 1])
        start = comma + 1
    pieces.append(text[start:])
    lines = [pieces[0]]
    width = column + len(pieces[0])
    for piece in pieces[1:]:
        piece = piece.lstrip(" ")
        if width + 1 + len(piece) <= WIDTH:
            lines[-1] += " " + piece
            width += 1 + len(piece)
        else:
            lines.append(continuation + piece)
            width = len(continuation) + len(piece)
    return "\n".join(lines)


# --- Laying out a line --------------------------------------------------------------------------

@dataclass
class Node:
    kind: str  # leaf, element, askama, root
    index: int = -1  # a leaf's token, or an element's or Askama block's opening token
    children: list = field(default_factory=list)


class Layout:
    """Lays out the tokens of a line. Output is built from placeholders, one per token, filled in at the end, so a trim marker decided late
    still reaches its token."""

    def __init__(self, tokens, boundaries, protected=frozenset()):
        self.tokens, self.boundaries = tokens, boundaries
        self.protected = protected  # source offsets where no line break may go (see `protect`)
        self.trims = {}  # token index -> set of sides
        self.rewritten = {}  # token index -> its text with its arguments wrapped

    # Trees and texts.

    def build(self, indices):
        root = Node("root")
        stack = [root]
        for index in indices:
            token = self.tokens[index]
            if token.kind == "start" and not token.self_closing and token.name not in VOID:
                node = Node("element", index)
                stack[-1].children.append(node)
                stack.append(node)
            elif token.kind == "end" and any(n.kind == "element" and self.tokens[n.index].name == token.name for n in stack[1:]):
                while not (stack[-1].kind == "element" and self.tokens[stack[-1].index].name == token.name):
                    stack.pop()
                stack[-1].children.append(Node("leaf", index))
                stack.pop()
            elif token.kind == "askama_block" and token.name in ASKAMA_OPENERS and token.name != "raw":
                node = Node("askama", index)
                stack[-1].children.append(node)
                stack.append(node)
            elif token.kind == "askama_block" and token.name in ASKAMA_CLOSERS and any(n.kind == "askama" for n in stack[1:]):
                while stack[-1].kind != "askama":
                    stack.pop()
                stack[-1].children.append(Node("leaf", index))
                stack.pop()
            else:
                stack[-1].children.append(Node("leaf", index))
        return root

    def leaves(self, node):
        if node.kind == "leaf":
            return [node.index]
        out = [node.index] if node.kind in ("element", "askama") else []
        for child in node.children:
            out.extend(self.leaves(child))
        return out

    def first(self, node):
        return self.leaves(node)[0]

    def last(self, node):
        return self.leaves(node)[-1]

    def width(self, nodes):
        return sum(len(self.tokens[i].text) for node in nodes for i in self.leaves(node))

    def out(self, nodes):
        return "".join(f"\x01{i}\x02" for node in nodes for i in self.leaves(node))

    def is_space(self, node):
        token = self.tokens[node.index] if node.kind == "leaf" else None
        return token is not None and token.kind == "text" and not token.text.strip()

    # Breaks.

    def can_break(self, left, right):
        """Whether adjacent tokens `left` and `right` (no whitespace between) may be split by a line break, and how: "trim" (a marker on an
        Askama tag) or "space" (invisible there)."""
        a, b = self.tokens[left], self.tokens[right]
        if (b.kind == "text" and b.text[:1] in ",.;:!?)") or (a.kind == "text" and a.text[-1:] == "("):
            return None  # possible, but harder to read than the long line
        if self.trim_side(left, right):
            return "trim"
        if self.tokens[left].end in self.protected:
            return None
        return "space" if self.boundaries[left].safe else None

    def trim_side(self, left, right):
        """Which tag a trim marker goes on to break between `left` and `right`, as (token index, side), or None. A marker removes all the
        whitespace on its side, so it never goes where the neighbor has whitespace of its own there."""
        a, b = self.tokens[left], self.tokens[right]
        if askama_side(b, "before") and not a.text[-1:].isspace():
            return right, "before"
        if askama_side(a, "after") and not b.text[:1].isspace():
            return left, "after"
        return None

    def take_break(self, left, right):
        how = self.can_break(left, right)
        if how == "trim":
            index, side = self.trim_side(left, right)
            self.trims.setdefault(index, set()).add(side)
        return how

    def runs(self, nodes, probe=False):
        """Sibling nodes cut where they may break. Whitespace already between two nodes may always become a line break, except where
        whitespace is kept."""
        runs, current = [], []
        for k, node in enumerate(nodes):
            if self.is_space(node) and current and k + 1 < len(nodes) and not self.boundaries[node.index].keeps \
                    and self.tokens[node.index].start not in self.protected:
                runs.append(current)
                current = []
                continue
            if current and not self.is_space(current[-1]) and self.can_break(self.last(current[-1]), self.first(node)):
                if not probe:
                    self.take_break(self.last(current[-1]), self.first(node))
                runs.append(current)
                current = []
            current.append(node)
        if current:
            runs.append(current)
        return runs

    # Lines.

    def render(self, nodes, indent, opened=False):
        """Nodes as lines: on one line if they fit, unless `opened` (the body of an element already broken open, whose children each get a
        line, as far as they may)."""
        if len(indent) + self.width(nodes) <= WIDTH and not (opened and len(self.runs(nodes, probe=True)) > 1):
            return [indent + self.out(nodes)]
        lines = []
        for run in self.runs(nodes):
            if len(run) == 1 and run[0].kind == "element" and len(indent) + self.width(run) > WIDTH:
                lines.extend(self.element(run[0], indent))
            elif len(run) == 1 and run[0].kind == "askama" and len(indent) + self.width(run) > WIDTH:
                lines.extend(self.askama(run[0], indent))
            else:
                lines.extend(self.glued(run, indent))
        return lines

    def element(self, node, indent):
        children = node.children
        closed = bool(children) and children[-1].kind == "leaf" and self.tokens[children[-1].index].kind == "end"
        inner = children[:-1] if closed else children
        head = self.start_tag(node.index, indent)
        if not inner:
            head[-1] += self.out(children)
            return head
        if not self.can_break(node.index, self.first(inner[0])):
            head[-1] += self.out(children)
            return head
        self.take_break(node.index, self.first(inner[0]))
        body = self.render(inner, indent + " " * STEP, opened=True)
        if closed and self.can_break(self.last(inner[-1]), children[-1].index):
            self.take_break(self.last(inner[-1]), children[-1].index)
            return head + body + [indent + self.out([children[-1]])]
        if closed:
            body[-1] += self.out([children[-1]])
        return head + body

    def askama(self, node, indent):
        children = node.children
        closed = bool(children) and children[-1].kind == "leaf" and self.tokens[children[-1].index].name in ASKAMA_CLOSERS
        inner = children[:-1] if closed else children
        if not inner or not self.can_break(node.index, self.first(inner[0])):
            return self.glued([node], indent)
        self.take_break(node.index, self.first(inner[0]))
        lines = self.glued([Node("leaf", node.index)], indent)
        group = []
        previous = node.index

        def flush():
            if group:
                lines.extend(self.render(list(group), indent + " " * STEP))
                group.clear()

        for child in inner:
            token = self.tokens[child.index]
            if child.kind == "leaf" and token.kind == "askama_block" and token.name in ASKAMA_MIDDLES \
                    and self.can_break(previous, child.index) and (not group or self.can_break(self.last(group[-1]), child.index)):
                if group:
                    self.take_break(self.last(group[-1]), child.index)
                flush()
                lines.extend(self.glued([child], indent))
                previous = child.index
                if child is not inner[-1]:
                    self.take_break(child.index, self.first(inner[inner.index(child) + 1]))
                continue
            group.append(child)
        if closed:
            if group and self.can_break(self.last(group[-1]), children[-1].index):
                self.take_break(self.last(group[-1]), children[-1].index)
                flush()
                lines.append(indent + self.out([children[-1]]))
            else:
                group.append(children[-1])
                flush()
        else:
            flush()
        return lines

    def start_tag(self, index, indent):
        """A start tag as lines: whole if it fits, otherwise its attributes wrapped one step in."""
        token = self.tokens[index]
        if (len(indent) + len(token.text) <= WIDTH and "\n" not in token.text) or not token.attrs:
            return [indent + f"\x01{index}\x02"]
        lines = [indent + token.tag_head]
        continuation = indent + " " * STEP
        for whitespace, unit, at in token.attrs:
            if not whitespace or at in self.protected:
                lines[-1] += whitespace + unit
            elif lines[-1] == indent + token.tag_head or len(lines[-1]) + 1 + len(unit) <= WIDTH:
                lines[-1] += " " + unit
            else:
                lines.append(continuation + unit)
        lines[-1] += token.tag_tail.strip()
        return lines

    def glued(self, nodes, indent):
        """Nodes that cannot be split: one line, but a long start tag's attributes wrapped, and the arguments of a long Askama tag that
        starts the line."""
        lines = [indent]
        for node in nodes:
            for index in self.leaves(node):
                token = self.tokens[index]
                column = self.visible(lines[-1])
                if token.kind == "start" and token.attrs and (column + len(token.text) > WIDTH or "\n" in token.text):
                    tag = self.start_tag(index, indent)
                    lines[-1] += tag[0][len(indent):]
                    lines.extend(tag[1:])
                    continue
                if token.kind in ("askama_block", "askama_expr") and column + len(token.text) > WIDTH \
                        and column == len(indent):
                    wrapped = wrap_arguments(token.text, column, indent + " " * STEP)
                    if wrapped != token.text:
                        self.rewritten[index] = wrapped
                lines[-1] += f"\x01{index}\x02"
        return lines

    def visible(self, line):
        """The column a line of placeholders reaches, counted from the last line break inside a rewritten tag, if there is one."""
        column = 0
        for k, piece in enumerate(re.split(r"\x01(\d+)\x02", line)):
            text = piece if k % 2 == 0 else self.rewritten.get(int(piece), self.tokens[int(piece)].text)
            column = len(text.rsplit("\n", 1)[1]) if "\n" in text else column + len(text)
        return column

    def fill(self, text):
        def token_text(match):
            index = int(match.group(1))
            out = self.rewritten.get(index, self.tokens[index].text)
            for side in self.trims.get(index, ()):
                out = with_trim(out, side)
            return out
        return re.sub(r"\x01(\d+)\x02", token_text, text)


def protect(tokens, fragments):
    """The source offsets inside any text in `fragments` as the template would write it, so a line break never splits what a test looks for
    in a page.

    The template's output is read as a graph: its HTML, a `{{ }}` standing for any text, and Askama's control flow, so a fragment may run
    through one branch of an `if` or `match` or through a loop's body any number of times."""
    nodes = []  # (kind, character or keyword, source offset): c (a character), w (an expression), m (a tag)
    for token in tokens:
        if token.kind == "askama_expr":
            nodes.append(("w", "", token.start))
        elif token.kind == "askama_block":
            nodes.append(("m", token.name, token.start))
        elif token.kind != "askama_comment":
            k = 0
            while k < len(token.text):
                kind, end = askama_at(token.text, k) if token.kind != "raw" else (None, k)
                if kind == "askama_expr":
                    nodes.append(("w", "", token.start + k))
                elif kind == "askama_block":
                    nodes.append(("m", keyword(token.text[k:end]), token.start + k))
                elif kind is None:
                    nodes.append(("c", token.text[k], token.start + k))
                    end = k + 1
                k = end
    jumps = control_flow(nodes)
    text = "".join(c if kind == "c" else "\x00" for kind, c, _ in nodes)
    protected = set()
    for fragment in fragments:
        anchor = fragment[:3]
        at = text.find(anchor)
        while at >= 0:
            end = match_from(nodes, jumps, at, fragment)
            if end is not None:
                for k in range(at, end):
                    protected.update(range(nodes[k][2] + 1, nodes[k + 1][2] + 1))
            at = text.find(anchor, at + 1)
    # A fragment may start inside what an expression writes, so it is also matched from each expression on, by at least ten characters of it
    # that follow the expression literally.
    by_piece = {}
    for fragment in fragments:
        for n in range(1, len(fragment) - 9):
            by_piece.setdefault(fragment[n:n + 10], []).append((fragment, n))
    for k, (kind, _, _) in enumerate(nodes):
        piece = text[k + 1:k + 11]
        if kind != "w" or "\x00" in piece:
            continue
        for fragment, n in by_piece.get(piece, ()):
            end = match_from(nodes, jumps, k + 1, fragment[n:])
            if end is not None:
                for m in range(k, end):
                    protected.update(range(nodes[m][2] + 1, nodes[m + 1][2] + 1))
    return protected


LOOPS = {"for"}
BRANCHES = {"if", "match"}
OPENERS = LOOPS | BRANCHES


def control_flow(nodes):
    """Where output may go next from each tag: into a branch or past it, round a loop again."""
    jumps = {}
    stack = []
    for k, (kind, word, _) in enumerate(nodes):
        if kind != "m":
            continue
        if word in OPENERS:
            stack.append((word, k, []))
        elif word in ("else", "elif", "when") and stack:
            stack[-1][2].append(k)
        elif word in ("endif", "endmatch", "endfor") and stack:
            opener, start, middles = stack.pop()
            jumps.setdefault(start, set()).update({k + 1 for k in middles} | {k})
            for middle in middles:
                jumps.setdefault(middle, set()).add(k)
            if opener in LOOPS:
                jumps.setdefault(k, set()).add(start + 1)
    return jumps


def match_from(nodes, jumps, at, fragment):
    """The last node of the longest match of `fragment` that starts at node `at`, or None. An expression stands for any characters, none
    included."""
    best = None
    seen = set()
    todo = [(at, 0)]
    while todo:
        k, j = todo.pop()
        if (k, j) in seen or k >= len(nodes):
            continue
        seen.add((k, j))
        kind, c, _ = nodes[k]
        if kind == "m":
            if nodes[k][1] in ("else", "elif", "when"):
                # The end of a branch: output goes on after the block.
                todo.extend((n, j) for n in jumps.get(k, ()))
                continue
            todo.append((k + 1, j))
            todo.extend((n, j) for n in jumps.get(k, ()))
            continue
        if kind == "w":
            for n in range(j, len(fragment) + 1):
                if n == len(fragment):
                    best = max(best or 0, k)
                else:
                    todo.append((k + 1, n))
            continue
        if fragment[j] == c:
            if j + 1 == len(fragment):
                best = max(best or 0, k)
            else:
                todo.append((k + 1, j + 1))
    return best if best is not None and best > at else None


def lay_out(src, classes, tags, fragments=()):
    tokens = tokenize(src)
    extends = bool(re.search(r"\{%-?\s*extends\b", src))
    layout = Layout(tokens, analyze(tokens, classes, tags, extends), protect(tokens, fragments))
    lines = src.split("\n")
    starts, at = [], 0
    for line in lines:
        starts.append(at)
        at += len(line) + 1
    output = []
    t = 0
    number = 0
    while number < len(lines):
        # A line and, when a start tag on it goes on to the next lines (its attributes wrapped), those lines too: laid out as one.
        last = number
        indent = re.match(r"[ \t]*", lines[number]).group(0)
        content = starts[number] + len(indent)
        while t < len(tokens) and tokens[t].end <= starts[number]:
            t += 1
        k = t
        while k < len(tokens) and tokens[k].start < starts[last] + len(lines[last]):
            token = tokens[k]
            if token.kind == "start" and token.start >= content and token.end > starts[last] + len(lines[last]):
                last = src.count("\n", 0, token.end)
            k += 1
        group = lines[number:last + 1]
        line_end = starts[last] + len(lines[last])
        following = last + 1
        if all(len(line) <= LIMIT for line in group) or group[0].lstrip().startswith("{#"):
            output.extend(group)
            number = following
            continue
        members, crossing = [], False
        k = t
        while k < len(tokens) and tokens[k].start < line_end:
            token = tokens[k]
            if token.start >= content and token.end <= line_end:
                members.append(k)
            elif token.kind == "text" and not token.text.strip():
                pass  # the indentation, or the line break after the line
            elif token.end > content:
                crossing = True
            k += 1
        trailing = group[-1][len(group[-1].rstrip()):]
        if crossing or not members or trailing:
            output.extend(group)
            number = following
            continue
        new = layout.render(layout.build(members).children, indent)
        output.append("\n".join(new))
        number = following
    return layout.fill("\n".join(output)) if layout.trims or "\x01" in "\n".join(output) else "\n".join(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--css", required=True)
    parser.add_argument("--check", action="store_true", help="list what would change, write nothing")
    parser.add_argument("--protect", help="a JSON list of texts no line break may split (what tests look for)")
    parser.add_argument("files", nargs="+")
    args = parser.parse_args()
    classes, tags = parse_css(open(args.css).read())
    fragments = json.load(open(args.protect)) if args.protect else []
    changed = 0
    for path in args.files:
        src = open(path).read()
        out = lay_out(src, classes, tags, fragments)
        if out != src:
            changed += 1
            if not args.check:
                open(path, "w").write(out)
            print(path)
    print(f"{changed} of {len(args.files)} files {'would change' if args.check else 'changed'}", file=sys.stderr)


if __name__ == "__main__":
    main()
