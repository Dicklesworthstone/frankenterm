import { createHistorySearch } from "./history-search.mjs";

function localTime(value) {
  if (!value) return "";
  // Native datetime-local values have no offset: Date uses the operator's
  // local timezone. Do not silently turn invalid controls into an all-time query.
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2})?$/.test(value)) throw new Error("Choose valid local start and end times.");
  const time = new Date(value).getTime();
  if (!Number.isSafeInteger(time) || time < 0) throw new Error("Choose valid local start and end times after 1970.");
  return String(time);
}

function captureLabel(ms) {
  const value = BigInt(ms);
  return value <= 8_640_000_000_000_000n
    ? new Date(Number(value)).toISOString() : `epoch milliseconds ${ms}`;
}

// Native controls remain separate from terminal-style key input. Result
// actions bind both query generation and exact pane identity, never row focus.
export function mountHistorySearch({
  document: doc = globalThis.document,
  readBody,
  inspectPane,
  inspectCapture,
  search = createHistorySearch({ readBody }),
} = {}) {
  const element = (id) => {
    const node = doc.getElementById(id);
    if (!node) throw new Error(`missing history control: ${id}`);
    return node;
  };
  const dialog = element("history-search");
  const opener = element("history-search-open");
  const form = element("history-search-form");
  const query = element("history-query");
  const mode = element("history-mode");
  const pane = element("history-pane");
  const since = element("history-since");
  const until = element("history-until");
  const limit = element("history-limit");
  const status = element("history-status");
  const results = element("history-results");
  const cancel = element("history-cancel");
  const closeButton = element("history-close");
  const openerLabel = opener.textContent;
  let opened = false;
  let returningToResults = false;
  let viewVersion = 0;
  let inputError = null;

  function hideResults() {
    opened = false;
    viewVersion += 1;
    results.replaceChildren();
    status.textContent = "";
    cancel.disabled = true;
    form.setAttribute("aria-busy", "false");
  }
  function reset() {
    hideResults();
    returningToResults = false;
    inputError = null;
    search.clear();
    query.value = pane.value = since.value = until.value = "";
    opener.textContent = openerLabel;
  }
  function close() {
    reset();
    if (dialog.open) dialog.close();
    opener.focus();
  }
  function leaveForResult() {
    // Retain one bounded search in memory while inspecting a hit. Reopening
    // restores the same ranking and exact capture identities without issuing
    // a new query. Explicit dismissal and page/session retirement still erase it.
    hideResults();
    returningToResults = true;
    opener.textContent = "Return to search results";
    if (dialog.open) dialog.close();
    opener.focus();
  }
  function render() {
    if (!opened) return;
    const renderedVersion = ++viewVersion;
    const state = search.snapshot();
    results.replaceChildren();
    form.setAttribute("aria-busy", String(state.status === "loading"));
    cancel.disabled = state.status !== "loading";
    if (state.status !== "ready") {
      status.textContent = inputError ?? state.error ?? (state.status === "loading" ? "Searching retained capture…"
        : state.status === "cancelled" ? "Search cancelled." : "Search all retained panes, or narrow by pane and capture time.");
      return;
    }
    const cap = state.hits.length >= state.criteria.limit ? " Requested result limit reached; narrow the query or raise the limit." : "";
    status.textContent = `${state.hits.length} ranked matches returned.${cap} This is not a total-match count; the server may apply a lower limit.`;
    for (const [index, hit] of state.hits.entries()) {
      const item = doc.createElement("li");
      const heading = doc.createElement("p");
      heading.textContent = `Pane ${hit.paneId} · segment ${hit.segmentId} · ${captureLabel(hit.capturedAt)}`;
      const snippet = doc.createElement("pre");
      snippet.textContent = hit.snippet || "No snippet supplied for this captured segment.";
      const historical = doc.createElement("button");
      historical.type = "button";
      historical.textContent = "Open captured segment";
      historical.addEventListener("click", () => {
        const selected = opened && viewVersion === renderedVersion && search.hit(index, state.version);
        if (!selected) return;
        const capture = { segmentId: selected.segmentId, paneId: selected.paneId,
          capturedAt: selected.capturedAt, contentLength: selected.contentLength };
        leaveForResult();
        inspectCapture(capture);
      });
      const inspect = doc.createElement("button");
      inspect.type = "button";
      inspect.textContent = `Open current output for pane ${hit.paneId}`;
      inspect.title = "This pane ID may belong to another session now. Opens new captured output only.";
      inspect.addEventListener("click", () => {
        const selected = opened && viewVersion === renderedVersion && search.hit(index, state.version);
        if (!selected) return;
        // End the modal before moving focus into the output inspector.
        const exactPaneId = selected.paneId;
        leaveForResult();
        inspectPane(exactPaneId);
      });
      item.append(heading, snippet, historical, inspect);
      results.append(item);
    }
  }
  function open() {
    if (opened) { query.focus(); return; }
    dialog.showModal();
    opened = true;
    returningToResults = false;
    render();
    query.focus();
  }
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!opened) return;
    let input;
    try {
      input = { query: query.value, mode: mode.value, paneId: pane.value.trim(),
        sinceMs: localTime(since.value), untilMs: localTime(until.value), limit: Number(limit.value) };
    } catch (error) {
      search.cancel();
      inputError = error.message;
      render();
      return;
    }
    inputError = null;
    const pending = search.search(input);
    render();
    void pending.then(render);
  });
  cancel.addEventListener("click", () => { search.cancel(); render(); });
  closeButton.addEventListener("click", close);
  dialog.addEventListener("cancel", (event) => { event.preventDefault(); close(); });
  // A result action hides the modal intentionally; external close() clears it.
  // Ignore queued close events once a newer dialog view has already opened.
  dialog.addEventListener("close", () => { if (!dialog.open && !returningToResults) reset(); });
  opener.addEventListener("click", open);
  opener.disabled = false;
  cancel.disabled = true;
  return { open, close, suspend: close };
}
