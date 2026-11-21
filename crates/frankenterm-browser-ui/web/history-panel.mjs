import { createHistoryLibrary, createHistorySearch } from "./history-search.mjs";

function localTime(value) {
  if (!value) return "";
  // Native datetime-local values have no offset: Date uses the operator's
  // local timezone. Do not silently turn invalid controls into an all-time query.
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:\.\d{1,3})?)?$/.test(value)) throw new Error("Choose valid local start and end times.");
  const time = new Date(value).getTime();
  if (!Number.isSafeInteger(time) || time < 0) throw new Error("Choose valid local start and end times after 1970.");
  return String(time);
}

function localTimeValue(ms) {
  if (ms === null) return "";
  if (BigInt(ms) > 8_640_000_000_000_000n) return null;
  const date = new Date(Number(ms));
  const year = date.getFullYear();
  if (year < 0 || year > 9999) return null;
  const pad = (value, width = 2) => String(value).padStart(width, "0");
  return `${pad(year, 4)}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}.${pad(date.getMilliseconds(), 3)}`;
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
  library = createHistoryLibrary({ readBody }),
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
  const libraryLoad = element("history-library-load");
  const libraryCancel = element("history-library-cancel");
  const libraryFilter = element("history-library-filter");
  const libraryStatus = element("history-library-status");
  const savedStatus = element("history-saved-status");
  const bookmarkStatus = element("history-bookmark-status");
  const savedList = element("history-saved-list");
  const bookmarkList = element("history-bookmark-list");
  const loadedSearch = element("history-loaded-search");
  const openerLabel = opener.textContent;
  let opened = false;
  let returningToResults = false;
  let viewVersion = 0;
  let libraryViewVersion = 0;
  let inputError = null;
  let savedSince = null;

  function hideResults() {
    opened = false;
    viewVersion += 1;
    results.replaceChildren();
    status.textContent = "";
    cancel.disabled = true;
    form.setAttribute("aria-busy", "false");
    libraryViewVersion += 1;
    savedList.replaceChildren();
    bookmarkList.replaceChildren();
    libraryStatus.textContent = savedStatus.textContent = bookmarkStatus.textContent = "";
  }
  function reset() {
    hideResults();
    returningToResults = false;
    inputError = null;
    search.clear();
    library.clear();
    savedSince = null;
    loadedSearch.textContent = "";
    libraryFilter.value = "";
    libraryLoad.textContent = "Load saved items";
    libraryLoad.disabled = false;
    libraryCancel.disabled = true;
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
  function useSavedSearch(saved) {
    const dateValue = localTimeValue(saved.sinceMs);
    if (saved.unavailable || dateValue === null) return;
    search.clear();
    inputError = null;
    query.value = saved.query;
    mode.value = "fts";
    pane.value = saved.paneId ?? "";
    since.value = dateValue;
    until.value = "";
    limit.value = saved.limit;
    // datetime-local cannot distinguish the two occurrences of a time when
    // daylight saving ends. Keep the exact stored instant until this field is
    // edited, including its millisecond precision.
    savedSince = { value: since.value, millis: saved.sinceMs ?? "" };
    const boundary = saved.sinceMs === null ? "all retained times"
      : `${saved.sinceMode === "last_run" ? "last recorded run" : "fixed start"}: ${captureLabel(saved.sinceMs)}`;
    loadedSearch.textContent = `Loaded “${saved.name}” in FTS mode, ${boundary}. Review the form and choose Search. This does not update its schedule or last-run marker.`;
    render();
    query.focus();
  }
  function renderLibrary() {
    if (!opened) return;
    const renderedVersion = ++libraryViewVersion;
    const state = library.snapshot();
    const filter = libraryFilter.value.slice(0, 256).trim().toLocaleLowerCase();
    const matches = (text) => !filter || text.toLocaleLowerCase().includes(filter);
    savedList.replaceChildren();
    bookmarkList.replaceChildren();
    savedStatus.textContent = bookmarkStatus.textContent = "";
    libraryLoad.disabled = state.status === "loading";
    libraryCancel.disabled = state.status !== "loading";
    libraryLoad.textContent = state.status === "idle" ? "Load saved items" : "Refresh saved items";
    libraryStatus.textContent = state.status === "loading" ? "Loading saved searches and bookmarks…"
      : state.status === "cancelled" ? "Loading cancelled."
        : state.status === "idle" ? "Load searches and pane bookmarks configured with ft." : "";
    if (state.status !== "ready") return;
    let shownSearches = 0;
    for (const [index, saved] of state.savedSearches.items.entries()) {
      if (!matches(`${saved.name} ${saved.query} ${saved.paneId ?? ""}`)) continue;
      shownSearches += 1;
      const item = doc.createElement("li");
      const heading = doc.createElement("p");
      heading.textContent = `${saved.name} · ${saved.paneId === null ? "All panes" : `Pane ${saved.paneId}`} · limit ${saved.limit}`;
      const text = doc.createElement("pre");
      text.textContent = saved.query;
      const use = doc.createElement("button");
      use.type = "button";
      use.textContent = "Use search";
      const unavailable = saved.unavailable ?? (localTimeValue(saved.sinceMs) === null
        ? "The saved start time is outside this viewer's date range. Review it with ft." : null);
      use.disabled = unavailable !== null;
      use.addEventListener("click", () => {
        const selected = opened && libraryViewVersion === renderedVersion && library.savedSearch(index, state.version);
        if (selected && !use.disabled) useSavedSearch(selected);
      });
      item.append(heading, text, use);
      if (unavailable || !saved.enabled) {
        const note = doc.createElement("p");
        note.textContent = unavailable ?? "Schedule disabled; available for manual search.";
        item.append(note);
      }
      savedList.append(item);
    }
    let shownBookmarks = 0;
    for (const [index, bookmark] of state.bookmarks.items.entries()) {
      if (!matches(`${bookmark.alias} ${bookmark.paneId} ${bookmark.tags.join(" ")} ${bookmark.description}`)) continue;
      shownBookmarks += 1;
      const item = doc.createElement("li");
      const heading = doc.createElement("p");
      heading.textContent = `${bookmark.alias} · Pane ${bookmark.paneId}`;
      const text = doc.createElement("p");
      text.textContent = [bookmark.tags.join(", "), bookmark.description].filter(Boolean).join(" · ");
      const inspect = doc.createElement("button");
      inspect.type = "button";
      inspect.textContent = "Open captured output";
      inspect.title = "Opens current captured output for this saved pane ID. The ID may belong to another session now.";
      inspect.addEventListener("click", () => {
        const selected = opened && libraryViewVersion === renderedVersion && library.bookmark(index, state.version);
        if (!selected) return;
        const exactPaneId = selected.paneId;
        leaveForResult();
        inspectPane(exactPaneId);
      });
      item.append(heading, text, inspect);
      bookmarkList.append(item);
    }
    savedStatus.textContent = state.savedSearches.error ?? `${shownSearches} of ${state.savedSearches.items.length} saved searches shown.`;
    bookmarkStatus.textContent = state.bookmarks.error ?? `${shownBookmarks} of ${state.bookmarks.items.length} bookmarks shown.`;
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
    renderLibrary();
    render();
    query.focus();
  }
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!opened) return;
    let input;
    try {
      input = { query: query.value, mode: mode.value, paneId: pane.value.trim(),
        sinceMs: savedSince?.value === since.value ? savedSince.millis : localTime(since.value),
        untilMs: localTime(until.value), limit: Number(limit.value) };
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
  libraryLoad.addEventListener("click", () => {
    if (!opened) return;
    const pending = library.load();
    renderLibrary();
    void pending.then(renderLibrary);
  });
  libraryCancel.addEventListener("click", () => { library.cancel(); renderLibrary(); });
  libraryFilter.addEventListener("input", renderLibrary);
  since.addEventListener("input", () => { savedSince = null; });
  since.addEventListener("change", () => { savedSince = null; });
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
