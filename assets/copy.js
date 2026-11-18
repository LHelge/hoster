// hoster's copy button for the trace export (ADR-0060, amended 2026-11-18): hoster's own
// script, no dependencies, no build step, loaded only on the Telemetry page's Traces tab.
//
// The markup is a plain link, `a[data-export]` (Open as text), which opens the export without
// this script. Where the browser has a clipboard, the script puts a Copy button in front of the
// link that fetches the same URL and puts its text on the clipboard; the link stays as it is.
// The outcome is said in the export's `[data-export-status]` live region for a moment, by text
// and class.
//
// CSP (ADR-0054): no inline scripts, no eval, no style= attributes, no markup parsed from text
// (a test in src/web/assets.rs checks this).
//
// The pure parts (the copy, and the idempotent insert of the buttons) are exported for
// `node --test assets/` (copy.test.mjs); the DOM wiring at the bottom runs only where a
// `document` exists. The buttons are put in again after every HTMX swap, and one delegated
// click listener on the document serves them all.

/** What the status region says after a copy, and the class it carries meanwhile. */
export const OUTCOMES = {
  copied: { text: "Copied", className: "is-copied" },
  failed: { text: "Copy failed", className: "is-failed" },
};

/** How long the outcome stays shown, in milliseconds. */
export const SHOWN_FOR = 2500;

/**
 * Puts the text at `url` on the clipboard. `env` holds the browser's `clipboard`
 * (`navigator.clipboard`), `fetch` and `ClipboardItem` (absent where unsupported).
 *
 * Where `clipboard.write` and `ClipboardItem` exist, the item is created before the fetch
 * finishes, with a promise of the text, so Safari keeps the user activation of the click across
 * the fetch. Otherwise, or if that write is refused, it fetches and then calls `writeText`. The
 * export is fetched at most once. Rejects if the fetch fails, answers other than 2xx, or the
 * clipboard refuses the text.
 */
export async function copyFrom(url, env) {
  const { clipboard, fetch, ClipboardItem } = env;
  let text;
  const load = () =>
    (text ??= fetch(url, { credentials: "same-origin" }).then((response) => {
      if (!response.ok) throw new Error(`the export answered ${response.status}`);
      return response.text();
    }));
  if (typeof ClipboardItem === "function" && typeof clipboard.write === "function") {
    try {
      const blob = load().then((t) => new Blob([t], { type: "text/plain" }));
      // Not awaited before the write: the promise goes into the item as it is.
      blob.catch(() => {});
      await clipboard.write([new ClipboardItem({ "text/plain": blob })]);
      return;
    } catch {
      // A browser that takes no promise in an item, or refused it: try the plain way. If the
      // fetch itself failed, `load` gives back the same rejection without fetching again.
    }
  }
  await clipboard.writeText(await load());
}

/** The class of the Copy button, which also marks a link as enhanced. */
export const BUTTON_CLASS = "trace-copy";

/**
 * Puts a Copy button in front of every `a[data-export]` under `root` that has none yet, and
 * returns how many it added. Running it again adds nothing, so it runs after every HTMX swap:
 * a boosted waterfall link morphs `main`, whose server markup has no button.
 */
export function offerCopy(root) {
  let added = 0;
  for (const link of root.querySelectorAll("a[data-export]")) {
    if (link.previousElementSibling?.classList.contains(BUTTON_CLASS)) continue;
    const button = link.ownerDocument.createElement("button");
    button.type = "button";
    button.className = `log-chip ${BUTTON_CLASS}`;
    button.textContent = "Copy";
    link.before(button);
    added += 1;
  }
  return added;
}

/** The pending timers that clear a status region, by region. */
const timers = new WeakMap();

/** Says `outcome` in `status` for a moment. */
function show(status, outcome) {
  clearTimeout(timers.get(status));
  status.classList.remove(OUTCOMES.copied.className, OUTCOMES.failed.className);
  status.classList.add(outcome.className);
  status.textContent = outcome.text;
  timers.set(
    status,
    setTimeout(() => {
      status.textContent = "";
      status.classList.remove(outcome.className);
    }, SHOWN_FOR),
  );
}

/**
 * One click listener on the document for every Copy button, so a swap that replaces a button
 * never loses its handler. The button copies its link's URL.
 */
function copyOnClick(event) {
  const button = event.target.closest?.(`button.${BUTTON_CLASS}`);
  const link = button?.nextElementSibling;
  if (!link?.matches("a[data-export]")) return;
  const env = {
    clipboard: navigator.clipboard,
    fetch: globalThis.fetch.bind(globalThis),
    ClipboardItem: globalThis.ClipboardItem,
  };
  copyFrom(link.href, env).then(
    () => true,
    () => false,
  ).then((ok) => {
    // Looked up when the copy ends: a swap meanwhile may have replaced the region.
    const status = link.isConnected
      ? link.closest("nav")?.querySelector("[data-export-status]")
      : link.ownerDocument.querySelector("[data-export-status]");
    if (status) show(status, ok ? OUTCOMES.copied : OUTCOMES.failed);
  });
}

if (typeof document !== "undefined" && typeof navigator !== "undefined" && navigator.clipboard) {
  offerCopy(document);
  document.addEventListener("click", copyOnClick);
  // htmx 4 names it `htmx:after:swap`, dispatched on the request's source element (or the
  // document once that is gone) and bubbling.
  document.addEventListener("htmx:after:swap", () => offerCopy(document));
}
