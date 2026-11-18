#!/usr/bin/env python3
"""Rewraps the prose of Askama comments (`{# ... #}`) so no line passes LIMIT characters.

Only a paragraph with a line over LIMIT is touched, and it is refilled at word boundaries to LIMIT, its lines starting where its text
starts, never inside an inline `{{ }}` or `{% %}`. A paragraph is a run of lines whose text starts at the same column, ended by a blank
line, a line that starts a list item (`- `, `* `, `1. `), or a code sample: a line whose text starts with `{%`, `{{` or `<`. Code samples
are never changed. Comments produce no output, so nothing the browser receives changes.

Usage: askama_comments.py [--check] FILE...
"""

import argparse
import re
import sys

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from askama_layout import tokenize  # noqa: E402

LIMIT = 140
BULLET = re.compile(r"(?:[-*]|\d+\.) ")


def is_code(text):
    return text.startswith(("{%", "{{", "<"))


INLINE_CODE = re.compile(r"\{\{.*?\}\}|\{%.*?%\}")


def words_of(text):
    """The words of prose, an inline `{{ }}` or `{% %}` kept whole. An HTML tag may still break between its attributes."""
    protected = INLINE_CODE.sub(lambda m: m.group(0).replace(" ", "\x00"), text)
    return [word.replace("\x00", " ") for word in protected.split()]


def reflow(src):
    lines = src.split("\n")
    changed = False
    for token in tokenize(src):
        if token.kind != "askama_comment":
            continue
        first = src.count("\n", 0, token.start)
        last = src.count("\n", 0, token.end)
        line_start = src.rfind("\n", 0, token.start) + 1
        if src[line_start:token.start].strip() or src[token.end:].split("\n", 1)[0].strip():
            continue  # the comment shares a line with code
        # Each line as (prefix, text): the prefix is the indentation, or `{# ` and what is before it.
        rows = []
        for n in range(first, last + 1):
            line = lines[n]
            if n == first:
                m = re.match(r"([ \t]*\{#-?[ \t]*)(.*)", line)
            else:
                m = re.match(r"([ \t]*)(.*)", line)
            rows.append((n, m.group(1), m.group(2)))
        paragraphs, current = [], []

        def close():
            if current:
                paragraphs.append(list(current))
                current.clear()

        for n, prefix, text in rows:
            column = len(prefix)
            if not text.strip() or is_code(text):
                close()
                continue
            if BULLET.match(text):
                close()
                current.append((n, prefix, text, column + BULLET.match(text).end()))
                continue
            if current and column != current[-1][3]:
                close()
            current.append((n, prefix, text, column))
        close()
        for paragraph in paragraphs:
            if all(len(lines[n]) <= LIMIT for n, _, _, _ in paragraph):
                continue
            words = words_of(" ".join(text for _, _, text, _ in paragraph))
            continuation = " " * paragraph[-1][3] if len(paragraph) > 1 else " " * paragraph[0][3]
            out = [paragraph[0][1]]
            fresh = True
            for word in words:
                if not fresh and len(out[-1]) + 1 + len(word) > LIMIT:
                    out.append(continuation)
                    fresh = True
                out[-1] += word if fresh else " " + word
                fresh = False
            if len(out) > 1 and out[-1].strip() in ("#}", "-#}"):
                # The closer alone on a line: the last word goes down with it.
                head, word = out[-2].rsplit(" ", 1)
                out[-2], out[-1] = head, continuation + word + " " + out[-1].strip()
            numbers = [n for n, _, _, _ in paragraph]
            lines[numbers[0]:numbers[-1] + 1] = [out[0]] + [None] * (len(numbers) - 1)
            lines[numbers[0]] = "\n".join(out)
            changed = True
    if not changed:
        return src
    return "\n".join(line for line in lines if line is not None)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--check", action="store_true", help="list what would change, write nothing")
    parser.add_argument("files", nargs="+")
    args = parser.parse_args()
    count = 0
    for path in args.files:
        src = open(path).read()
        out = reflow(src)
        if out != src:
            count += 1
            if not args.check:
                open(path, "w").write(out)
            print(path)
    print(f"{count} of {len(args.files)} files {'would change' if args.check else 'changed'}", file=sys.stderr)


if __name__ == "__main__":
    main()
