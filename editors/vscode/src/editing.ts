// Askama editing conveniences as plain text transformations, kept apart from the VS Code API so they can be tested on their own
// (editing.test.ts). extension.ts applies them to editors.

/** The block tags Surround with Tag offers, with the placeholder for their argument. */
export const SURROUND_TAGS: ReadonlyArray<{ tag: string; argument: string; closer: string }> = [
  { tag: "if", argument: "condition", closer: "endif" },
  { tag: "for", argument: "item in items", closer: "endfor" },
  { tag: "block", argument: "name", closer: "endblock" },
  { tag: "macro", argument: "name()", closer: "endmacro" },
  { tag: "call", argument: "name()", closer: "endcall" },
  { tag: "filter", argument: "name", closer: "endfilter" },
  { tag: "match", argument: "value", closer: "endmatch" },
  { tag: "let", argument: "name", closer: "endlet" },
  { tag: "raw", argument: "", closer: "endraw" },
];

/**
 * Comments out lines with `{# … #}`, or uncomments them if they already are: the first line starts with `{#` and the last ends with `#}`,
 * ignoring indentation and trailing spaces.
 */
export function toggleComment(lines: readonly string[]): string[] {
  if (lines.length === 0) {
    return [];
  }
  const first = lines[0];
  const last = lines[lines.length - 1];
  const commented = first.trimStart().startsWith("{#") && last.trimEnd().endsWith("#}");
  const result = [...lines];
  if (commented) {
    const indent = first.length - first.trimStart().length;
    result[0] = first.slice(0, indent) + first.slice(indent + 2).replace(/^ /, "");
    const lastLine = result[result.length - 1];
    const end = lastLine.trimEnd();
    result[result.length - 1] = end.slice(0, -2).replace(/ $/, "") + lastLine.slice(end.length);
    return result;
  }
  const indent = first.length - first.trimStart().length;
  result[0] = first.slice(0, indent) + "{# " + first.slice(indent);
  const lastLine = result[result.length - 1];
  const end = lastLine.trimEnd();
  result[result.length - 1] = end + " #}" + lastLine.slice(end.length);
  return result;
}

/** What auto-closing does after a keystroke: replace `remove` characters after the cursor with `insert`, then put the cursor `cursorAfter` characters after where it was. */
export interface AutoClose {
  remove: number;
  insert: string;
  cursorAfter: number;
}

const OPENERS: ReadonlyArray<{ open: string; close: string }> = [
  { open: "{%", close: "%}" },
  { open: "{{", close: "}}" },
  { open: "{#", close: "#}" },
];

/**
 * After a keystroke that leaves `before` to the left of the cursor and `after` to its right: if it just completed `{%`, `{{` or `{#`, the
 * edit that turns it into `{% | %}` (the cursor at `|`). The editor may already have auto-closed `{` with `}`, so up to as many `}` as the
 * opener has braces are replaced. Nothing if the closer is already there or the opener is part of a longer run of braces.
 */
export function autoClose(before: string, after: string): AutoClose | undefined {
  const opener = OPENERS.find(({ open }) => before.endsWith(open));
  if (opener === undefined || before.endsWith("{" + opener.open)) {
    return undefined;
  }
  const braces = (opener.open.match(/\{/g) ?? []).length;
  const autoClosed = Math.min((after.match(/^\}*/) ?? [""])[0].length, braces);
  const rest = after.slice(autoClosed);
  // The editor's auto-closed braces look like `}}` themselves, so only a closer after them counts as one already there.
  if (rest.trimStart().startsWith(opener.close)) {
    return undefined;
  }
  return { remove: autoClosed, insert: `  ${opener.close}`, cursorAfter: 1 };
}

/** Escapes text for a snippet, where `$`, `}` and `\` mean something. */
export function escapeSnippet(text: string): string {
  return text.replace(/[\\$}]/g, (character) => "\\" + character);
}

/**
 * The snippet that wraps `selected` in `tag`'s pair. Text within one line is wrapped inline; whole lines get the tags on lines of their
 * own, at the first line's indentation.
 */
export function surround(tag: string, selected: string, wholeLines: boolean): string {
  const entry = SURROUND_TAGS.find((candidate) => candidate.tag === tag);
  if (entry === undefined) {
    return escapeSnippet(selected);
  }
  const argument = entry.argument === "" ? "" : ` \${1:${entry.argument}}`;
  const opener = `{% ${entry.tag}${argument} %}`;
  const closer = `{% ${entry.closer} %}`;
  const inner = entry.tag === "match" ? "{% when ${2:pattern} %}" : "";
  if (!wholeLines) {
    return `${opener}${inner}${escapeSnippet(selected)}${closer}`;
  }
  const indent = selected.match(/^[ \t]*/)?.[0] ?? "";
  const body = selected.endsWith("\n") ? selected : selected + "\n";
  const innerLine = inner === "" ? "" : `${indent}${inner}\n`;
  return `${indent}${opener}\n${innerLine}${escapeSnippet(body)}${indent}${closer}\n`;
}
