#!/usr/bin/env python3
"""Rewraps the prose of Rust, TypeScript and Python comments to fill lines up to LIMIT characters.

A comment is a run of lines that hold only a comment with the same marker and indentation: `//`, `///` or `//!`, or the ` * ` lines of a
`/** */` block, in Rust and TypeScript; `#` in Python, along with docstrings (a triple-quoted string that opens a module or follows a `def`
or `class` line). Its prose is split into paragraphs, ended by a blank comment line, and each paragraph is refilled at word boundaries,
never inside an inline `code span`. A list item (`- `, `* `, `1. `) starts its own paragraph, its later lines indented under its text. Left
exactly as they are: fenced code blocks, lines indented as code, tables, headings and rules, `Usage:` lines, and comments that follow code
on the same line.

Usage: rewrap_comments.py [--check] FILE...
"""

import argparse
import re
import sys

LIMIT = 140
LINE_COMMENT = re.compile(r"^([ \t]*)(//[/!]?)(?: (.*)|())$")
PYTHON_COMMENT = re.compile(r"^([ \t]*)(#)(?: (.*)|())$")
DOCSTRING_START = re.compile(r'^([ \t]*)"""(.*)$')
BLOCK_START = re.compile(r"^[ \t]*/\*\*\s*$")
BLOCK_LINE = re.compile(r"^([ \t]*) \*(?: (.*)|())$")
BLOCK_END = re.compile(r"^[ \t]*\*/")
BULLET = re.compile(r"^( *)((?:[-*]|\d+\.) +)")
CODE_SPAN = re.compile(r"`[^`]*`")


def words_of(text):
    """The words of prose, each inline code span kept whole."""
    protected = CODE_SPAN.sub(lambda m: m.group(0).replace(" ", "\x00"), text)
    return [word.replace("\x00", " ") for word in protected.split()]


def fill(first_prefix, continuation_prefix, words):
    """Lines of `words`, the first after `first_prefix`, the rest after `continuation_prefix`."""
    lines = [first_prefix]
    fresh = True
    for word in words:
        if not fresh and len(lines[-1]) + 1 + len(word) > LIMIT:
            lines.append(continuation_prefix)
            fresh = True
        lines[-1] += word if fresh else " " + word
        fresh = False
    return lines


def rewrap_group(prefix, texts):
    """The lines of one comment, given its prefix (indentation and marker plus a space) and the text of each line after the prefix."""
    out = []
    paragraph = None  # (lead, continuation indent, [texts])
    fenced = False

    def close():
        nonlocal paragraph
        if paragraph is None:
            return
        lead, indent, lines = paragraph
        words = words_of(" ".join(line.strip() for line in lines))
        out.extend(line.rstrip() for line in fill(prefix + lead, prefix + " " * indent, words))
        paragraph = None

    def verbatim(text):
        close()
        out.append((prefix + text).rstrip() if text else prefix.rstrip())

    for text in texts:
        stripped = text.strip()
        if fenced or stripped.startswith("```"):
            if stripped.startswith("```"):
                fenced = not fenced
            verbatim(text)
            continue
        if not stripped or stripped.startswith(("|", "#", "---", "<", "Usage:")):
            verbatim(text)
            continue
        bullet = BULLET.match(text)
        indent = len(text) - len(text.lstrip(" "))
        if bullet:
            close()
            lead = bullet.group(1) + bullet.group(2)
            paragraph = (lead, len(lead), [text[len(lead):]])
        elif paragraph is not None and indent == paragraph[1]:
            paragraph[2].append(text)
        elif indent >= 4 and (paragraph is None or indent > paragraph[1]):
            verbatim(text)
        else:
            close()
            paragraph = (" " * indent, indent, [text[indent:]])
    close()
    return out


def opens_docstring(out):
    """Whether a triple-quoted string on the next line is a docstring: the first statement of the module, or the first in the body of a
    `def` or `class` whose header ends the lines so far."""
    code = [line for line in out if line.strip() and not line.lstrip().startswith("#")]
    if not code:
        return True
    if not code[-1].rstrip().endswith(":"):
        return False
    # The header may span lines; it starts at the nearest `def` or `class` line, and no line after that one may end a block of its own.
    for at in range(len(code) - 1, max(len(code) - 20, -1), -1):
        if re.match(r"\s*(async\s+)?(def|class)\b", code[at]):
            return all(not line.rstrip().endswith(":") for line in code[at:-1])
    return False


def rewrap(src, python=False):
    lines = src.split("\n")
    out = []
    i = 0
    line_comment = PYTHON_COMMENT if python else LINE_COMMENT
    while i < len(lines):
        line = lines[i]
        docstring = DOCSTRING_START.match(line) if python else None
        if docstring and opens_docstring(out):
            indent, rest = docstring.group(1), docstring.group(2)
            if '"""' in rest:
                # A docstring on one line stays on one line when it fits.
                end = i
            else:
                end = next((j for j in range(i + 1, len(lines)) if '"""' in lines[j]), None)
            if end is None:
                out.append(line)
                i += 1
                continue
            body = [line[len(indent):]] + [text[len(indent):] if text.startswith(indent) else text.strip() for text in lines[i + 1:end + 1]]
            closer_alone = end > i and body[-1].strip() == '"""'
            texts = body[:-1] if closer_alone else body
            if end == i and len(line) <= LIMIT:
                out.append(line)
            else:
                out.extend(rewrap_group(indent, texts))
            if closer_alone:
                out.append(indent + '"""')
            i = end + 1
            continue
        m = line_comment.match(line)
        if m:
            indent, marker = m.group(1), m.group(2)
            texts = []
            while i < len(lines):
                n = line_comment.match(lines[i])
                if not n or n.group(1) != indent or n.group(2) != marker:
                    break
                texts.append(n.group(3) if n.group(3) is not None else "")
                i += 1
            out.extend(rewrap_group(f"{indent}{marker} ", texts))
            continue
        if not python and BLOCK_START.match(line):
            out.append(line)
            i += 1
            texts, indent = [], None
            while i < len(lines) and not BLOCK_END.match(lines[i]):
                n = BLOCK_LINE.match(lines[i])
                if not n or (indent is not None and n.group(1) != indent):
                    break
                indent = n.group(1)
                texts.append(n.group(2) if n.group(2) is not None else "")
                i += 1
            if indent is not None:
                out.extend(rewrap_group(f"{indent} * ", texts))
            continue
        out.append(line)
        i += 1
    return "\n".join(out)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--check", action="store_true", help="list what would change, write nothing")
    parser.add_argument("files", nargs="+")
    args = parser.parse_args()
    count = 0
    for path in args.files:
        src = open(path).read()
        out = rewrap(src, python=path.endswith(".py"))
        if out != src:
            count += 1
            if not args.check:
                open(path, "w").write(out)
            print(path)
    print(f"{count} of {len(args.files)} files {'would change' if args.check else 'changed'}", file=sys.stderr)


if __name__ == "__main__":
    main()
