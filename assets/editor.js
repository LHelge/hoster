// hoster's stack editor (ADR-0060): hoster's own script, no dependencies, no build step.
//
// It enhances every `textarea[data-editor="toml"]` with a highlighted copy of its text laid
// behind it, a line-number gutter and a few editing keys. The textarea stays the real control:
// caret, selection, undo and the form value are the browser's, and without this script the page
// is a plain form. The tokenizer only colours text; it never decides whether a stack file is
// valid, which is the server's job.
//
// CSP (ADR-0054): no inline scripts, no eval, no style= attributes, and markup is never parsed
// from text: the highlight is built from DOM nodes (a test in src/web/assets.rs checks this).
// The script moves the highlight and the gutter with element.style (CSSOM), which `style-src
// 'self'` allows.
//
// The pure parts are exported for `node --test assets/` (editor.test.mjs); the DOM wiring at the
// bottom runs only where a `document` exists.

/** Spaces one Tab inserts, and Shift+Tab removes. */
export const INDENT = "  ";

// ---- Tokenizer ---------------------------------------------------------------------------------

const BARE_KEY = /[A-Za-z0-9_-]+/y;
const DATE_TIME =
  /\d{4}-\d{2}-\d{2}(?:[Tt ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:[Zz]|[+-]\d{2}:\d{2})?)?|\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?/y;
const NUMBER =
  /[+-]?(?:0x[0-9A-Fa-f](?:_?[0-9A-Fa-f])*|0o[0-7](?:_?[0-7])*|0b[01](?:_?[01])*|inf|nan|\d(?:_?\d)*(?:\.\d(?:_?\d)*)?(?:[eE][+-]?\d(?:_?\d)*)?)/y;
const BOOL = /true|false/y;
const ESCAPE = /\\(?:u[0-9A-Fa-f]{4}|U[0-9A-Fa-f]{8}|[btnfre"\\]|[ \t]*\r?\n)/y;
// The start of a `key =` line (bare, dotted or quoted keys without spaces).
const KEY_ASSIGN = /[A-Za-z0-9_."'-]+[ \t]*=/y;
// What may follow a value without making it part of a longer bare word.
const VALUE_END = /[\s,\]}#=]/;
// A run of anything else up to the next separator: shown as plain text.
const JUNK = /[^\s,[\]{}#="']+/y;

/** The match of sticky `re` at `pos` in `text`, or null. */
function matchAt(re, text, pos) {
  re.lastIndex = pos;
  const m = re.exec(text);
  return m ? m[0] : null;
}

/**
 * Splits `text` into `{kind, text}` tokens for highlighting. The kinds are `comment`, `table`
 * (a `[table]` or `[[array]]` header), `key`, `string`, `escape` (inside a basic string),
 * `number`, `bool`, `date`, `punct` and `plain` (whitespace and anything that isn't valid TOML).
 *
 * It never throws, and joining the tokens' text always gives back `text` exactly.
 */
export function tokenize(text) {
  const tokens = [];
  const push = (kind, part) => {
    if (part === "") return;
    const last = tokens[tokens.length - 1];
    if (last && last.kind === kind) last.text += part;
    else tokens.push({ kind, text: part });
  };
  // Open arrays (`[`) and inline tables (`{`), innermost last.
  const nesting = [];
  // Whether the next word is a key: at the start of a top-level line, and after `{` or a `,`
  // inside an inline table.
  let expectKey = true;
  let atLineStart = true;
  let i = 0;
  const n = text.length;

  // A string starting at `i` with `quote` ("'" or '"'); `multi` for the triple-quoted kinds.
  const string = (quote, multi, kind) => {
    const open = multi ? quote.repeat(3) : quote;
    const basic = quote === '"';
    let j = i + open.length;
    let run = j;
    push(kind, open);
    while (j < n) {
      if (!multi && (text[j] === "\n" || text[j] === "\r")) break;
      if (basic && text[j] === "\\") {
        const esc = matchAt(ESCAPE, text, j);
        if (esc && (multi || !esc.includes("\n"))) {
          if (kind === "string") {
            push(kind, text.slice(run, j));
            push("escape", esc);
            run = j + esc.length;
          }
          j += esc.length;
          continue;
        }
        // An invalid escape: skip the next character so `\"` never closes the string, but never
        // skip a line break.
        j += text[j + 1] === "\n" || text[j + 1] === "\r" ? 1 : 2;
        continue;
      }
      if (text.startsWith(open, j)) {
        let end = j + open.length;
        // A multi-line string may end with one or two quotes of its own before the delimiter.
        if (multi) {
          for (let extra = 0; extra < 2 && text[end] === quote; extra += 1) end += 1;
        }
        push(kind, text.slice(run, end));
        i = end;
        return;
      }
      j += 1;
    }
    j = Math.min(j, n);
    push(kind, text.slice(run, j));
    i = j;
  };

  // A `[table]` or `[[array]]` header from `i` to its closing bracket(s), honouring quoted keys.
  const header = () => {
    const double = text.startsWith("[[", i);
    let j = i + (double ? 2 : 1);
    const close = double ? "]]" : "]";
    while (j < n && text[j] !== "\n") {
      const c = text[j];
      if (c === '"' || c === "'") {
        j += 1;
        while (j < n && text[j] !== c && text[j] !== "\n") {
          j += c === '"' && text[j] === "\\" && text[j + 1] !== "\n" ? 2 : 1;
        }
        j = Math.min(j + 1, n);
        continue;
      }
      if (text.startsWith(close, j)) {
        j += close.length;
        break;
      }
      j += 1;
    }
    push("table", text.slice(i, j));
    i = j;
  };

  while (i < n) {
    const c = text[i];
    if (c === "\n" || c === "\r") {
      const len = c === "\r" && text[i + 1] === "\n" ? 2 : 1;
      push("plain", text.slice(i, i + len));
      i += len;
      atLineStart = true;
      if (nesting.length === 0) expectKey = true;
      continue;
    }
    if (c === " " || c === "\t") {
      let j = i;
      while (j < n && (text[j] === " " || text[j] === "\t")) j += 1;
      push("plain", text.slice(i, j));
      i = j;
      continue;
    }
    const lineStart = atLineStart;
    atLineStart = false;
    if (c === "#") {
      let j = i;
      while (j < n && text[j] !== "\n" && text[j] !== "\r") j += 1;
      push("comment", text.slice(i, j));
      i = j;
      continue;
    }
    // An array left open while typing would swallow every line below it. Arrays hold values, so
    // a line starting with `key =` means the array was never closed: start afresh.
    if (lineStart && nesting[nesting.length - 1] === "[" && matchAt(KEY_ASSIGN, text, i)) {
      nesting.length = 0;
      expectKey = true;
    }
    if (c === "[" && lineStart && nesting.length === 0) {
      header();
      continue;
    }
    const top = nesting[nesting.length - 1];
    if (expectKey) {
      if (c === '"' || c === "'") {
        string(c, false, "key");
        continue;
      }
      const bare = matchAt(BARE_KEY, text, i);
      if (bare) {
        push("key", bare);
        i += bare.length;
        continue;
      }
      if (c === ".") {
        push("punct", c);
        i += 1;
        continue;
      }
      if (c === "=") {
        push("punct", c);
        i += 1;
        expectKey = false;
        continue;
      }
      if (c === "}" && top === "{") {
        nesting.pop();
        push("punct", c);
        i += 1;
        expectKey = false;
        continue;
      }
      if (c === ",") {
        push("punct", c);
        i += 1;
        continue;
      }
    } else {
      if (c === '"' || c === "'") {
        const multi = text.startsWith(c.repeat(3), i);
        string(c, multi, "string");
        continue;
      }
      if (c === "[" || c === "{") {
        nesting.push(c);
        push("punct", c);
        i += 1;
        expectKey = c === "{";
        continue;
      }
      if (c === "]" || c === "}") {
        if (top === (c === "]" ? "[" : "{")) nesting.pop();
        push("punct", c);
        i += 1;
        continue;
      }
      if (c === ",") {
        push("punct", c);
        i += 1;
        if (top === "{") expectKey = true;
        continue;
      }
      if (c === "=") {
        push("punct", c);
        i += 1;
        continue;
      }
      const word =
        (() => {
          const date = matchAt(DATE_TIME, text, i);
          if (date) return ["date", date];
          const bool = matchAt(BOOL, text, i);
          if (bool) return ["bool", bool];
          const number = matchAt(NUMBER, text, i);
          if (number) return ["number", number];
          return null;
        })();
      if (word) {
        const [kind, value] = word;
        const next = text[i + value.length];
        if (next === undefined || VALUE_END.test(next)) {
          push(kind, value);
          i += value.length;
          continue;
        }
      }
    }
    // Anything else: not TOML we can colour. Take a run of it as plain text.
    const junk = matchAt(JUNK, text, i);
    const len = junk ? junk.length : 1;
    push("plain", text.slice(i, i + len));
    i += len;
  }
  return tokens;
}

// ---- Editing helpers ---------------------------------------------------------------------------

/** The number of lines in `text`: one more than its newlines. */
export function lineCount(text) {
  let lines = 1;
  for (let i = text.indexOf("\n"); i !== -1; i = text.indexOf("\n", i + 1)) lines += 1;
  return lines;
}

/** The offsets of the start of the line holding `start` and the end of the line holding `end`. */
function lineSpan(text, start, end) {
  const from = text.lastIndexOf("\n", start - 1) + 1;
  // A selection that ends just after a newline doesn't include the next line.
  const last = end > start && text[end - 1] === "\n" ? end - 1 : end;
  const nl = text.indexOf("\n", last);
  return [from, nl === -1 ? text.length : nl];
}

/**
 * What Tab does to the selection `start..end` of `text`: an edit `{from, to, insert}` replacing
 * `text.slice(from, to)` with `insert`, and the selection afterwards (`selStart`, `selEnd`).
 * Without a selection, or within one line, it inserts `INDENT` at the caret (replacing a
 * selection); a selection over several lines indents each of them.
 */
export function indent(text, start, end) {
  const [from, to] = lineSpan(text, start, end);
  if (start === end || !text.slice(start, end).includes("\n")) {
    return { from: start, to: end, insert: INDENT, selStart: start + INDENT.length, selEnd: start + INDENT.length };
  }
  const lines = text.slice(from, to).split("\n");
  const insert = lines.map((line) => (line === "" ? line : INDENT + line)).join("\n");
  const firstGrew = lines[0] === "" ? 0 : INDENT.length;
  return { from, to, insert, selStart: start + firstGrew, selEnd: end + (insert.length - (to - from)) };
}

/**
 * What Shift+Tab does: removes up to one `INDENT` of leading spaces (or one tab) from each line
 * the selection touches. Returns `null` when no line has any to remove.
 */
export function outdent(text, start, end) {
  const [from, to] = lineSpan(text, start, end);
  const lines = text.slice(from, to).split("\n");
  const removed = lines.map((line) => {
    if (line.startsWith("\t")) return 1;
    let k = 0;
    while (k < INDENT.length && line[k] === " ") k += 1;
    return k;
  });
  if (removed.every((k) => k === 0)) return null;
  const insert = lines.map((line, idx) => line.slice(removed[idx])).join("\n");
  const firstLine = start - from;
  const selStart = start - Math.min(removed[0], firstLine);
  const selEnd = Math.max(selStart, end - ((to - from) - insert.length));
  return { from, to, insert, selStart, selEnd };
}

/** What Enter inserts at `pos`: a newline and the current line's leading whitespace. */
export function newline(text, pos) {
  const from = text.lastIndexOf("\n", pos - 1) + 1;
  let k = from;
  while (k < pos && (text[k] === " " || text[k] === "\t")) k += 1;
  return "\n" + text.slice(from, k);
}

// ---- DOM wiring --------------------------------------------------------------------------------

/** One enhanced textarea. */
class Editor {
  constructor(textarea) {
    this.textarea = textarea;
    this.lines = 0;
    this.leaveOnTab = false;
    this.frame = 0;
    this.build();
    this.render();
    textarea.addEventListener("input", () => this.schedule());
    textarea.addEventListener("scroll", () => this.follow());
    textarea.addEventListener("keydown", (e) => this.key(e));
    // A resized textarea (its handle, or the window) changes what the gutter must cover.
    if (typeof ResizeObserver === "function") {
      new ResizeObserver(() => this.follow()).observe(textarea);
    }
  }

  build() {
    const ta = this.textarea;
    const doc = ta.ownerDocument;
    const root = doc.createElement("div");
    root.className = "editor";
    const gutter = doc.createElement("div");
    gutter.className = "editor-gutter";
    gutter.setAttribute("aria-hidden", "true");
    const numbers = doc.createElement("div");
    numbers.className = "editor-lines";
    gutter.append(numbers);
    const body = doc.createElement("div");
    body.className = "editor-body";
    const pre = doc.createElement("pre");
    pre.className = "editor-highlight";
    pre.setAttribute("aria-hidden", "true");
    const code = doc.createElement("code");
    pre.append(code);
    const hint = doc.createElement("p");
    hint.className = "editor-hint";
    hint.id = `${ta.id || "editor"}-keys`;
    hint.textContent = "Tab indents, Shift+Tab outdents. Press Esc, then Tab, to leave the editor.";

    ta.before(root);
    body.append(pre, ta);
    root.append(gutter, body);
    root.after(hint);
    ta.classList.add("editor-input");
    ta.setAttribute("wrap", "off");
    const described = ta.getAttribute("aria-describedby");
    ta.setAttribute("aria-describedby", described ? `${described} ${hint.id}` : hint.id);

    Object.assign(this, { root, gutter, numbers, pre, code });
  }

  /** Redraws at the next frame, so a burst of input costs one tokenize. */
  schedule() {
    if (this.frame) return;
    this.frame = requestAnimationFrame(() => {
      this.frame = 0;
      this.render();
    });
  }

  render() {
    const text = this.textarea.value;
    const doc = this.textarea.ownerDocument;
    const fragment = doc.createDocumentFragment();
    for (const { kind, text: part } of tokenize(text)) {
      if (kind === "plain") {
        fragment.append(part);
      } else {
        const span = doc.createElement("span");
        span.className = `tok-${kind}`;
        span.textContent = part;
        fragment.append(span);
      }
    }
    // A final newline only shows as a line if something follows it.
    if (text.endsWith("\n") || text === "") fragment.append(" ");
    this.code.replaceChildren(fragment);
    this.renumber(lineCount(text));
    this.follow();
  }

  renumber(lines) {
    if (lines === this.lines) return;
    const doc = this.textarea.ownerDocument;
    while (this.numbers.childElementCount < lines) {
      const line = doc.createElement("span");
      const no = this.numbers.childElementCount + 1;
      line.dataset.line = String(no);
      line.textContent = String(no);
      this.numbers.append(line);
    }
    while (this.numbers.childElementCount > lines) this.numbers.lastElementChild.remove();
    this.lines = lines;
    // The gutter is as wide as the largest number, plus its padding.
    this.gutter.style.setProperty("--digits", String(String(lines).length));
  }

  /** Keeps the highlight and the gutter under the textarea's scroll position. */
  follow() {
    const { scrollTop, scrollLeft } = this.textarea;
    this.pre.style.transform = `translate(${-scrollLeft}px, ${-scrollTop}px)`;
    this.numbers.style.transform = `translateY(${-scrollTop}px)`;
  }

  /**
   * Marks `lines` (1-based numbers) in the gutter with `severity` ("error" or "warning"),
   * replacing earlier marks of that severity. Used by the live diagnostics.
   */
  markLines(lines, severity) {
    const marked = new Set(lines.map(Number));
    for (const line of this.numbers.children) {
      line.classList.toggle(`is-${severity}`, marked.has(Number(line.dataset.line)));
    }
  }

  key(e) {
    const leave = this.leaveOnTab;
    this.leaveOnTab = false;
    if (e.isComposing || e.altKey || e.ctrlKey || e.metaKey) return;
    const ta = this.textarea;
    const { value, selectionStart: start, selectionEnd: end } = ta;
    if (e.key === "Escape") {
      this.leaveOnTab = true;
      return;
    }
    if (e.key === "Tab") {
      if (leave) return; // let the browser move focus
      e.preventDefault();
      const edit = e.shiftKey ? outdent(value, start, end) : indent(value, start, end);
      if (edit) this.apply(edit);
      return;
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      const insert = newline(value, start);
      this.apply({ from: start, to: end, insert, selStart: start + insert.length, selEnd: start + insert.length });
    }
  }

  /**
   * Replaces `from..to` with `insert` so that the browser's undo can take it back, then sets the
   * selection. `execCommand("insertText")` keeps the undo history in Chromium, Firefox and
   * Safari; where it is missing or refuses, `setRangeText` does the edit without it.
   */
  apply({ from, to, insert, selStart, selEnd }) {
    const ta = this.textarea;
    ta.setSelectionRange(from, to);
    const doc = ta.ownerDocument;
    let done = false;
    try {
      done = from !== to || insert !== "" ? doc.execCommand("insertText", false, insert) : true;
    } catch {
      done = false;
    }
    if (!done) {
      ta.setRangeText(insert, from, to, "end");
      ta.dispatchEvent(new Event("input", { bubbles: true }));
    }
    ta.setSelectionRange(selStart, selEnd);
  }
}

/** The editors on this page, by textarea. */
const editors = new WeakMap();

/** Enhances every not yet enhanced `textarea[data-editor="toml"]` under `root`. */
export function enhance(root) {
  for (const ta of root.querySelectorAll('textarea[data-editor="toml"]')) {
    if (!editors.has(ta)) editors.set(ta, new Editor(ta));
  }
}

/** The editor for `textarea`, if it was enhanced (for the live diagnostics). */
export function editorFor(textarea) {
  return editors.get(textarea);
}

if (typeof document !== "undefined") {
  enhance(document);
}
