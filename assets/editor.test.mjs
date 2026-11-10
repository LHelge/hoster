// Tests for the pure parts of editor.js (ADR-0060). Run with `node --test assets/`; no install.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { INDENT, indent, lineCount, newline, offsetAt, outdent, tokenize } from "./editor.js";

/** The tokens of `text` that aren't plain, as `kind:text` strings. */
function coloured(text) {
  return tokenize(text)
    .filter((t) => t.kind !== "plain")
    .map((t) => `${t.kind}:${t.text}`);
}

/** Tokenizing `text` and joining the tokens gives back `text`. */
function assertRoundTrip(text) {
  const joined = tokenize(text).map((t) => t.text).join("");
  assert.equal(joined, text);
}

test("comments, headers, keys and punctuation", () => {
  assert.deepEqual(coloured('# a stack\nname = "web" # trailing\n[services.web]\n[[routes]]\n'), [
    "comment:# a stack",
    "key:name",
    "punct:=",
    'string:"web"',
    "comment:# trailing",
    "table:[services.web]",
    "table:[[routes]]",
  ]);
});

test("dotted and quoted keys", () => {
  assert.deepEqual(coloured('a.b = 1\n"quoted key" = 2\n\'lit\'.x = 3\n'), [
    "key:a",
    "punct:.",
    "key:b",
    "punct:=",
    "number:1",
    'key:"quoted key"',
    "punct:=",
    "number:2",
    "key:'lit'",
    "punct:.",
    "key:x",
    "punct:=",
    "number:3",
  ]);
});

test("a header with a quoted key holding a bracket", () => {
  assert.deepEqual(coloured('[services."a]b"]\nx = 1\n'), [
    'table:[services."a]b"]',
    "key:x",
    "punct:=",
    "number:1",
  ]);
});

test("basic strings and their escapes", () => {
  assert.deepEqual(coloured('a = "x\\ty\\u00e9\\"z"\n'), [
    "key:a",
    "punct:=",
    'string:"x',
    "escape:\\t",
    "string:y",
    // Adjacent tokens of one kind are merged into one span.
    "escape:\\u00e9\\\"",
    'string:z"',
  ]);
});

test("an invalid escape neither closes the string nor eats the line break", () => {
  assert.deepEqual(coloured('a = "x\\q"\nb = "c\\\nd = 1\n'), [
    "key:a",
    "punct:=",
    'string:"x\\q"',
    "key:b",
    "punct:=",
    'string:"c\\',
    "key:d",
    "punct:=",
    "number:1",
  ]);
});

test("literal strings have no escapes", () => {
  assert.deepEqual(coloured("a = 'C:\\path\\n'\n"), ["key:a", "punct:=", "string:'C:\\path\\n'"]);
});

test("multi-line strings run across lines", () => {
  const basic = 'a = """\nline "one"\\\n  two"""\nb = 1\n';
  assert.deepEqual(coloured(basic), [
    "key:a",
    "punct:=",
    'string:"""\nline "one"',
    "escape:\\\n",
    'string:  two"""',
    "key:b",
    "punct:=",
    "number:1",
  ]);
  const literal = "a = '''\n# not a comment\n[not.a.table]\n'''\nb = true\n";
  assert.deepEqual(coloured(literal), [
    "key:a",
    "punct:=",
    "string:'''\n# not a comment\n[not.a.table]\n'''",
    "key:b",
    "punct:=",
    "bool:true",
  ]);
});

test("a multi-line string may end with extra quotes", () => {
  assert.deepEqual(coloured('a = """x""""\n'), ["key:a", "punct:=", 'string:"""x""""']);
});

test("an unterminated string ends at the line break (single) or the end (multi)", () => {
  assert.deepEqual(coloured('a = "open\nb = 1\n'), [
    "key:a",
    "punct:=",
    'string:"open',
    "key:b",
    "punct:=",
    "number:1",
  ]);
  assert.deepEqual(coloured("a = '''open\nb = 1\n"), ["key:a", "punct:=", "string:'''open\nb = 1\n"]);
});

test("numbers", () => {
  const values = ["1", "+17", "-5", "1_000", "3.14", "-0.01", "5e+22", "1e06", "6.626e-34", "0xDEAD_beef", "0o755", "0b1101", "inf", "+inf", "-nan"];
  for (const value of values) {
    assert.deepEqual(coloured(`a = ${value}\n`), ["key:a", "punct:=", `number:${value}`], value);
  }
});

test("booleans, dates and times", () => {
  assert.deepEqual(coloured("a = true\nb = false\n"), ["key:a", "punct:=", "bool:true", "key:b", "punct:=", "bool:false"]);
  for (const value of ["1979-05-27T07:32:00Z", "1979-05-27T00:32:00.999999-07:00", "1979-05-27 07:32:00", "1979-05-27", "07:32:00", "00:32:00.5"]) {
    assert.deepEqual(coloured(`a = ${value}\n`), ["key:a", "punct:=", `date:${value}`], value);
  }
});

test("a word that only starts like a value is plain", () => {
  assert.deepEqual(coloured("a = trueish\nb = 12abc\n"), ["key:a", "punct:=", "key:b", "punct:="]);
});

test("arrays and inline tables", () => {
  // Adjacent punctuation is one token (`]]`, `[{`).
  assert.deepEqual(coloured('a = [1, "two", [3]]\nb = { c = 1, d.e = "f" }\ng = [{ h = 1 }, {}]\n'), [
    "key:a", "punct:=", "punct:[", "number:1", "punct:,", 'string:"two"', "punct:,", "punct:[", "number:3", "punct:]]",
    "key:b", "punct:=", "punct:{", "key:c", "punct:=", "number:1", "punct:,", "key:d", "punct:.", "key:e", "punct:=", 'string:"f"', "punct:}",
    "key:g", "punct:=", "punct:[{", "key:h", "punct:=", "number:1", "punct:},", "punct:{}]",
  ]);
});

test("a multi-line array keeps its values, comments included", () => {
  assert.deepEqual(coloured('a = [\n  "x", # first\n  "y",\n]\nb = 1\n'), [
    "key:a", "punct:=", "punct:[", 'string:"x"', "punct:,", "comment:# first", 'string:"y"', "punct:,", "punct:]",
    "key:b", "punct:=", "number:1",
  ]);
});

test("an array left open doesn't swallow the lines after it", () => {
  assert.deepEqual(coloured('a = [1,\nb = "x"\n[next]\n'), [
    "key:a", "punct:=", "punct:[", "number:1", "punct:,",
    "key:b", "punct:=", 'string:"x"',
    "table:[next]",
  ]);
});

test("CRLF line endings", () => {
  assert.deepEqual(coloured("a = 1\r\n[t]\r\nb = 2\r\n"), ["key:a", "punct:=", "number:1", "table:[t]", "key:b", "punct:=", "number:2"]);
});

test("round trip: joined tokens give back the input", () => {
  const cases = [
    "",
    "\n",
    "a",
    "=",
    '"',
    "'''",
    '"""\\',
    "[",
    "[[",
    "a = [",
    "a = {",
    "a = }]]",
    "a = \"\\",
    "\t\t# tabs\r\n",
    "\u{1F600} = \"é\"\n",
    "x = 1 y = 2 z\n",
    '[a."b\\"c"]\n',
    "]]]{{{,,,===\"'#",
  ];
  for (const text of cases) assertRoundTrip(text);
});

test("round trip and no plain keys or values in the stack-format examples", () => {
  const doc = readFileSync(new URL("../site/content/wiki/stack-format.md", import.meta.url), "utf8");
  const blocks = [...doc.matchAll(/```toml\n([\s\S]*?)```/g)].map((m) => m[1]);
  assert.ok(blocks.length > 3, "stack-format.md has TOML examples");
  for (const block of blocks) {
    assertRoundTrip(block);
    // Valid TOML leaves nothing plain but whitespace (`…` placeholders aside).
    const junk = tokenize(block).filter((t) => t.kind === "plain" && t.text.trim() !== "" && !t.text.includes("…"));
    assert.deepEqual(junk, [], block);
  }
});

test("round trip on random input never throws", () => {
  const alphabet = ['"', "'", "[", "]", "{", "}", "=", ",", ".", "#", "\\", "\n", "\r", " ", "a", "1", "-", ":", "t", "e"];
  let seed = 42;
  const random = () => {
    seed = (seed * 1103515245 + 12345) % 2 ** 31;
    return seed / 2 ** 31;
  };
  for (let round = 0; round < 2000; round += 1) {
    const len = Math.floor(random() * 40);
    let text = "";
    for (let k = 0; k < len; k += 1) text += alphabet[Math.floor(random() * alphabet.length)];
    assertRoundTrip(text);
  }
});

test("lineCount", () => {
  assert.equal(lineCount(""), 1);
  assert.equal(lineCount("a"), 1);
  assert.equal(lineCount("a\n"), 2);
  assert.equal(lineCount("a\nb\nc"), 3);
});

/** Applies an edit from `indent`/`outdent` to `text`. */
function applied(text, edit) {
  return text.slice(0, edit.from) + edit.insert + text.slice(edit.to);
}

test("Tab without a selection inserts the indent at the caret", () => {
  const edit = indent("ab", 1, 1);
  assert.equal(applied("ab", edit), `a${INDENT}b`);
  assert.deepEqual([edit.selStart, edit.selEnd], [3, 3]);
});

test("Tab over a selection within one line replaces it", () => {
  const edit = indent("abcd", 1, 3);
  assert.equal(applied("abcd", edit), `a${INDENT}d`);
  assert.deepEqual([edit.selStart, edit.selEnd], [3, 3]);
});

test("Tab over several lines indents each non-empty line and keeps the selection on them", () => {
  const text = "a = 1\n\nb = 2\nc = 3";
  // From the middle of line 1 to the middle of line 3.
  const edit = indent(text, 2, 9);
  assert.equal(applied(text, edit), "  a = 1\n\n  b = 2\nc = 3");
  assert.deepEqual([edit.selStart, edit.selEnd], [4, 13]);
});

test("a selection ending just after a newline doesn't indent the next line", () => {
  const text = "a\nb\nc";
  const edit = indent(text, 0, 4);
  assert.equal(applied(text, edit), "  a\n  b\nc");
});

test("Shift+Tab removes up to one indent or one tab per line", () => {
  const text = "    a\n b\n\tc\nd";
  const edit = outdent(text, 0, text.length);
  assert.equal(applied(text, edit), "  a\nb\nc\nd");
  assert.equal(edit.selStart, 0);
  assert.equal(edit.selEnd, text.length - 4);
});

test("Shift+Tab on the caret's line moves the caret with the text", () => {
  const text = "x\n  key = 1";
  const edit = outdent(text, 6, 6);
  assert.equal(applied(text, edit), "x\nkey = 1");
  assert.deepEqual([edit.selStart, edit.selEnd], [4, 4]);
});

test("Shift+Tab with the caret inside the indent stops at the line start", () => {
  const text = "  a";
  const edit = outdent(text, 1, 1);
  assert.equal(applied(text, edit), "a");
  assert.deepEqual([edit.selStart, edit.selEnd], [0, 0]);
});

test("Shift+Tab with nothing to remove does nothing", () => {
  assert.equal(outdent("a\nb", 0, 3), null);
});

test("Enter keeps the current line's indent", () => {
  assert.equal(newline("[a]\n  key = 1", 13), "\n  ");
  assert.equal(newline("key = 1", 7), "\n");
  assert.equal(newline("\t\tx", 3), "\n\t\t");
  // In the middle of the indent, only the part before the caret is kept.
  assert.equal(newline("    x", 2), "\n  ");
});

test("offsetAt maps a diagnostic's line and column to a textarea offset", () => {
  const text = "a = 1\nname = \"x\"\n\tkey = 2\n";
  assert.equal(offsetAt(text, 1, 1), 0);
  assert.equal(offsetAt(text, 2, 1), 6);
  assert.equal(offsetAt(text, 2, 8), 13);
  // A tab is one character.
  assert.equal(offsetAt(text, 3, 2), text.indexOf("key"));
  // Past the end of the line or the text, it clamps.
  assert.equal(offsetAt(text, 1, 99), 5);
  assert.equal(offsetAt(text, 99, 1), text.length);
});

test("offsetAt counts characters, not UTF-16 units", () => {
  // The emoji is one character (two UTF-16 units); é is one either way.
  const text = 'a = "\u{1F600}é" # x\n';
  assert.equal(offsetAt(text, 1, 7), text.indexOf("é"));
  assert.equal(offsetAt(text, 1, 10), text.indexOf("#"));
});
