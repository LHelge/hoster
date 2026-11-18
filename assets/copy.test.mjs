// Tests for the pure part of copy.js (ADR-0060). Run with `node --test assets/`; no install.

import assert from "node:assert/strict";
import { test } from "node:test";

import { BUTTON_CLASS, copyFrom, OUTCOMES, offerCopy } from "./copy.js";

const URL = "/telemetry/traces/export?sort=slowest";

/** A `fetch` that answers `status` with `body` and counts its calls. */
function fakeFetch(body, status = 200) {
  const calls = [];
  const fetch = (url, init) => {
    calls.push({ url, init });
    return Promise.resolve({ ok: status >= 200 && status < 300, status, text: async () => body });
  };
  return { fetch, calls };
}

/** A `ClipboardItem` that keeps what it was given. */
class FakeItem {
  constructor(items) {
    this.items = items;
  }
}

/** A clipboard whose `write` reads each item's promise, as a browser does. */
function fakeClipboard({ write = true, refuseWrite = false } = {}) {
  const clipboard = { written: [], plain: [] };
  if (write) {
    clipboard.write = async (items) => {
      if (refuseWrite) throw new TypeError("promises are not supported");
      for (const item of items) {
        const blob = await item.items["text/plain"];
        clipboard.written.push({ type: blob.type, text: await blob.text() });
      }
    };
  }
  clipboard.writeText = async (text) => {
    clipboard.plain.push(text);
  };
  return clipboard;
}

test("writes a ClipboardItem holding the fetch's promise where it can", async () => {
  const { fetch, calls } = fakeFetch("hoster trace export\n");
  const clipboard = fakeClipboard();
  let itemMadeBeforeFetchSettled = false;
  class Item extends FakeItem {
    constructor(items) {
      super(items);
      itemMadeBeforeFetchSettled = items["text/plain"] instanceof Promise;
    }
  }
  await copyFrom(URL, { clipboard, fetch, ClipboardItem: Item });
  assert.ok(itemMadeBeforeFetchSettled);
  assert.deepEqual(clipboard.written, [{ type: "text/plain", text: "hoster trace export\n" }]);
  assert.deepEqual(clipboard.plain, []);
  assert.deepEqual(calls, [{ url: URL, init: { credentials: "same-origin" } }]);
});

test("the write is called in the same task as the click, before the fetch settles", () => {
  let wrote = false;
  const clipboard = {
    write: () => {
      wrote = true;
      return Promise.resolve();
    },
    writeText: async () => {},
  };
  const fetch = () => new Promise(() => {});
  copyFrom(URL, { clipboard, fetch, ClipboardItem: FakeItem });
  assert.ok(wrote, "clipboard.write ran synchronously");
});

test("falls back to fetch then writeText without ClipboardItem", async () => {
  const { fetch, calls } = fakeFetch("text");
  const clipboard = fakeClipboard();
  await copyFrom(URL, { clipboard, fetch, ClipboardItem: undefined });
  assert.deepEqual(clipboard.plain, ["text"]);
  assert.deepEqual(clipboard.written, []);
  assert.equal(calls.length, 1);
});

test("falls back to writeText without clipboard.write", async () => {
  const { fetch } = fakeFetch("text");
  const clipboard = fakeClipboard({ write: false });
  await copyFrom(URL, { clipboard, fetch, ClipboardItem: FakeItem });
  assert.deepEqual(clipboard.plain, ["text"]);
});

test("falls back to writeText when the write is refused, fetching once", async () => {
  const { fetch, calls } = fakeFetch("text");
  const clipboard = fakeClipboard({ refuseWrite: true });
  await copyFrom(URL, { clipboard, fetch, ClipboardItem: FakeItem });
  assert.deepEqual(clipboard.plain, ["text"]);
  assert.equal(calls.length, 1);
});

test("rejects when the export answers an error, fetching once and writing nothing", async () => {
  for (const ClipboardItem of [FakeItem, undefined]) {
    const { fetch, calls } = fakeFetch("The trace list cannot be read right now.\n", 503);
    const clipboard = fakeClipboard();
    await assert.rejects(copyFrom(URL, { clipboard, fetch, ClipboardItem }), /503/);
    assert.deepEqual(clipboard.written, []);
    assert.deepEqual(clipboard.plain, []);
    assert.equal(calls.length, 1);
  }
});

test("rejects when the fetch fails", async () => {
  const fetch = () => Promise.reject(new TypeError("network"));
  await assert.rejects(
    copyFrom(URL, { clipboard: fakeClipboard(), fetch, ClipboardItem: undefined }),
    /network/,
  );
});

test("rejects when the clipboard refuses the text", async () => {
  const { fetch } = fakeFetch("text");
  const clipboard = fakeClipboard({ write: false });
  clipboard.writeText = async () => {
    throw new Error("NotAllowedError");
  };
  await assert.rejects(copyFrom(URL, { clipboard, fetch }), /NotAllowedError/);
});

test("the outcomes are said by text and class", () => {
  assert.equal(OUTCOMES.copied.text, "Copied");
  assert.equal(OUTCOMES.failed.text, "Copy failed");
  assert.notEqual(OUTCOMES.copied.className, OUTCOMES.failed.className);
});

/** A minimal element: a tag, classes, and siblings in its parent's `children`. */
class FakeElement {
  constructor(doc, tag, className = "") {
    this.ownerDocument = doc;
    this.tagName = tag;
    this.className = className;
    this.parent = null;
  }
  get classList() {
    return { contains: (name) => this.className.split(" ").includes(name) };
  }
  get previousElementSibling() {
    const siblings = this.parent.children;
    return siblings[siblings.indexOf(this) - 1] ?? null;
  }
  before(node) {
    const siblings = this.parent.children;
    node.parent = this.parent;
    siblings.splice(siblings.indexOf(this), 0, node);
  }
}

/** A nav holding the export links, as the template renders it, with `createElement`. */
function fakeNav() {
  const doc = { createElement: (tag) => new FakeElement(doc, tag) };
  const nav = { children: [] };
  const links = [
    new FakeElement(doc, "a", "log-chip"),
    new FakeElement(doc, "a", "log-chip trace-export-text"),
  ];
  for (const el of links) {
    el.parent = nav;
    nav.children.push(el);
  }
  const link = nav.children[1];
  nav.querySelectorAll = (selector) => {
    assert.equal(selector, "a[data-export]");
    return [link];
  };
  return { nav, link };
}

test("offerCopy puts one Copy button in front of the export link, however often it runs", () => {
  const { nav, link } = fakeNav();
  assert.equal(offerCopy(nav), 1);
  assert.equal(offerCopy(nav), 0);
  assert.equal(nav.children.length, 3);
  const button = link.previousElementSibling;
  assert.equal(button.tagName, "button");
  assert.equal(button.type, "button");
  assert.equal(button.className, `log-chip ${BUTTON_CLASS}`);
  assert.equal(button.textContent, "Copy");
  assert.equal(nav.children[2], link, "the link stays, after the button");
});

test("offerCopy adds the button again once a swap has dropped it", () => {
  const { nav, link } = fakeNav();
  offerCopy(nav);
  // A morph to the server's markup, which has no button.
  nav.children.splice(nav.children.indexOf(link) - 1, 1);
  assert.equal(offerCopy(nav), 1);
  assert.ok(link.previousElementSibling.classList.contains(BUTTON_CLASS));
});
