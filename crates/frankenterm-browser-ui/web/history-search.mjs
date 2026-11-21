// Search retained capture through the existing read-only /search API.
// Results identify database segments, never authority to control a live pane.
const U64_MAX = 18_446_744_073_709_551_615n;
const I64_MAX = 9_223_372_036_854_775_807n;
const MAX_BODY_CHARS = 8 * 1024 * 1024;
const MAX_SNIPPET_CHARS = 16 * 1024;
const MAX_SAVED_ITEMS = 500;

function decimal(value, maximum = I64_MAX) {
  if (typeof value !== "string" || !/^[0-9]{1,20}$/.test(value)) return null;
  const parsed = BigInt(value);
  return parsed <= maximum ? parsed.toString() : null;
}

function criteriaFor({ query, mode = "phrase", paneId = "", sinceMs = "", untilMs = "", limit = 50 } = {}) {
  if (typeof query !== "string" || !query.trim() || query.length > 2048 || query.includes("\0")) {
    throw new Error("Enter a query of 1–2048 characters.");
  }
  if (!["phrase", "fts"].includes(mode)) throw new Error("Choose phrase or FTS query mode.");
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > 200) throw new Error("Result limit must be 1–200.");
  const optional = (value, maximum, message) => {
    if (value === "") return null;
    const parsed = decimal(value, maximum);
    if (parsed === null) throw new Error(message);
    return parsed;
  };
  const pane = optional(paneId, U64_MAX, "Pane ID must be an unsigned 64-bit integer.");
  const since = optional(sinceMs, I64_MAX, "Invalid start time.");
  const until = optional(untilMs, I64_MAX, "Invalid end time.");
  if (since !== null && until !== null && BigInt(since) > BigInt(until)) {
    throw new Error("Start time must not follow end time.");
  }
  return Object.freeze({ query: query.trim(), mode, paneId: pane, sinceMs: since, untilMs: until, limit });
}

function searchPath(criteria) {
  // FTS5 quoted phrases escape embedded quotes by doubling them. This is a
  // tokenized phrase search, NOT an exact byte substring search. Raw FTS is opt-in.
  const q = criteria.mode === "phrase" ? `"${criteria.query.replaceAll('"', '""')}"` : criteria.query;
  const params = new URLSearchParams({ q, limit: String(criteria.limit) });
  for (const [key, value] of [["pane_id", criteria.paneId], ["since", criteria.sinceMs], ["until", criteria.untilMs]]) {
    if (value !== null) params.set(key, value);
  }
  return `search?${params}`;
}

function decodedBody(body) {
  if (typeof body !== "string" || body.length > MAX_BODY_CHARS) throw new Error("invalid response");
  const value = JSON.parse(body);
  // Preserve exact integer identities but retain ORIGINAL JSON types for
  // validation. Numeric snippet/error fields must not turn into accepted text.
  const exact = JSON.parse(body.replace(
    /"(?:[^"\\]|\\.)*"|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/g,
    (token) => /^-?\d+$/.test(token) ? JSON.stringify(token) : token,
  ));
  return { value, exact };
}

function decodeResults(body, criteria) {
  const { value, exact } = decodedBody(body);
  if (value?.ok !== true || !Array.isArray(value.data?.results)
      || value.data.results.length > criteria.limit || value.data.total !== value.data.results.length) {
    throw new Error("invalid response");
  }
  const seen = new Set();
  return Object.freeze(value.data.results.map((row, index) => {
    const raw = exact.data.results[index];
    const integer = (key, max = I64_MAX) => typeof row?.[key] === "number" ? decimal(raw?.[key], max) : null;
    const segmentId = integer("segment_id");
    const paneId = integer("pane_id", U64_MAX);
    const capturedAt = integer("captured_at");
    const contentLength = integer("content_len", U64_MAX);
    if (segmentId === null || segmentId === "0" || paneId === null || capturedAt === null || contentLength === null
        || typeof row.score !== "number" || !Number.isFinite(row.score)
        || (row.snippet != null && (typeof row.snippet !== "string" || row.snippet.length > MAX_SNIPPET_CHARS))
        || seen.has(segmentId) || (criteria.paneId !== null && criteria.paneId !== paneId)
        || (criteria.sinceMs !== null && BigInt(capturedAt) < BigInt(criteria.sinceMs))
        || (criteria.untilMs !== null && BigInt(capturedAt) > BigInt(criteria.untilMs))) {
      throw new Error("invalid hit");
    }
    seen.add(segmentId);
    return Object.freeze({ segmentId, paneId, capturedAt, contentLength, score: row.score, snippet: row.snippet ?? "" });
  }));
}

function savedCollection(body, key) {
  const { value, exact } = decodedBody(body);
  if (value?.ok !== true || !Array.isArray(value.data?.[key])
      || value.data[key].length > MAX_SAVED_ITEMS || value.data.total !== value.data[key].length) {
    throw new Error("invalid saved collection");
  }
  return { rows: value.data[key], rawRows: exact.data[key] };
}

function savedText(value, maximum, allowEmpty = false) {
  if (typeof value !== "string" || value.length > maximum || value.includes("\0")
      || (!allowEmpty && !value.trim())) throw new Error("invalid saved text");
  return value;
}

function savedInteger(row, raw, key, maximum = I64_MAX, optional = false) {
  if (optional && row?.[key] == null) return null;
  const value = typeof row?.[key] === "number" ? decimal(raw?.[key], maximum) : null;
  if (value === null) throw new Error("invalid saved integer");
  return value;
}

function decodeSavedSearches(body) {
  const { rows, rawRows } = savedCollection(body, "saved_searches");
  const seen = new Set();
  return Object.freeze(rows.map((row, index) => {
    const id = savedText(row?.id, 256);
    const name = savedText(row.name, 1024);
    const query = savedText(row.query, 2048);
    const paneId = savedInteger(row, rawRows[index], "pane_id", U64_MAX, true);
    const limit = savedInteger(row, rawRows[index], "limit");
    const sinceMode = savedText(row.since_mode, 64);
    const fixed = savedInteger(row, rawRows[index], "since_ms", I64_MAX, true);
    const lastRun = savedInteger(row, rawRows[index], "last_run_at", I64_MAX, true);
    if (limit === "0" || typeof row.enabled !== "boolean" || seen.has(id)) throw new Error("invalid saved search");
    seen.add(id);
    // Stored queries use the CLI's FTS semantics. A last_run search uses its
    // recorded run boundary, not since_ms or the browser's current clock.
    const sinceMs = sinceMode === "fixed" ? fixed : lastRun;
    const unavailable = !["fixed", "last_run"].includes(sinceMode)
      ? "This saved search uses a time mode the viewer does not support. Review it with ft."
      : BigInt(limit) > 200n
        ? `The saved result limit is ${limit}; this viewer supports at most 200. Review the saved search with ft.` : null;
    return Object.freeze({ id, name, query, paneId, limit, sinceMode, sinceMs, enabled: row.enabled, unavailable });
  }));
}

function decodeBookmarks(body) {
  const { rows, rawRows } = savedCollection(body, "bookmarks");
  const seen = new Set();
  return Object.freeze(rows.map((row, index) => {
    const paneId = savedInteger(row, rawRows[index], "pane_id", U64_MAX);
    const alias = savedText(row.alias, 1024);
    if (seen.has(alias) || !Array.isArray(row.tags) || row.tags.length > 64) throw new Error("invalid bookmark");
    seen.add(alias);
    const tags = Object.freeze(row.tags.map((tag) => savedText(tag, 256)));
    const description = row.description == null ? "" : savedText(row.description, 4096, true);
    return Object.freeze({ paneId, alias, tags, description });
  }));
}

// The existing GET endpoints expose one local operator's configured items.
// Loading them grants no input or live-attachment authority, and a browser
// search does not advance the saved search's scheduler/last-run metadata.
export function createHistoryLibrary({ readBody, setTimer = globalThis.setTimeout, clearTimer = globalThis.clearTimeout, timeoutMs = 10_000 } = {}) {
  if (typeof readBody !== "function" || !Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 60_000) {
    throw new Error("invalid saved-history dependencies or deadline");
  }
  const empty = () => Object.freeze({ status: "idle", items: Object.freeze([]), error: null });
  let version = 0;
  let flight = null;
  let state = Object.freeze({ version, status: "idle", savedSearches: empty(), bookmarks: empty() });
  function retire(status) {
    version += 1;
    const old = flight;
    flight = null;
    if (old) { clearTimer(old.timer); old.controller.abort(); }
    state = Object.freeze({ version, status, savedSearches: empty(), bookmarks: empty() });
  }
  function item(key, index, expectedVersion) {
    return state.status === "ready" && state.version === expectedVersion && Number.isSafeInteger(index)
      ? state[key].items[index] ?? null : null;
  }
  return {
    async load() {
      retire("loading");
      const batch = { controller: new AbortController(), timer: null };
      flight = batch;
      let onAbort;
      const cancelled = new Promise((_, reject) => {
        onAbort = () => reject(new Error("saved-history deadline"));
        batch.controller.signal.addEventListener("abort", onAbort, { once: true });
      });
      batch.timer = setTimer(() => batch.controller.abort(), timeoutMs);
      try {
        const responses = await Promise.allSettled([
          ["saved-searches", decodeSavedSearches], ["bookmarks", decodeBookmarks],
        ].map(([path, decode]) => Promise.race([
          Promise.resolve().then(() => {
            if (batch.controller.signal.aborted) throw new Error("saved-history cancelled");
            return readBody(path, batch.controller.signal);
          }).then(decode), cancelled,
        ])));
        if (flight !== batch) return false;
        const collection = (result, label) => result.status === "fulfilled"
          ? Object.freeze({ status: "ready", items: result.value, error: null })
          : Object.freeze({ status: "error", items: Object.freeze([]), error: batch.controller.signal.aborted
            ? `${label} timed out. Refresh saved items to try again.`
            : `${label} could not be loaded or exceeded the viewer's bounds. Refresh saved items or review them with ft.` });
        state = Object.freeze({ version, status: "ready",
          savedSearches: collection(responses[0], "Saved searches"), bookmarks: collection(responses[1], "Bookmarks") });
        return responses.every((result) => result.status === "fulfilled");
      } finally {
        clearTimer(batch.timer);
        batch.controller.signal.removeEventListener("abort", onAbort);
        if (flight === batch) flight = null;
      }
    },
    cancel() { retire("cancelled"); },
    clear() { retire("idle"); },
    snapshot() { return state; },
    savedSearch(index, expectedVersion) { return item("savedSearches", index, expectedVersion); },
    bookmark(index, expectedVersion) { return item("bookmarks", index, expectedVersion); },
  };
}

// readBody is the host's existing bounded, abort-aware HTTP reader. Search
// shares its byte limit/UTF-8/credential policy rather than adding a fetch stack.
export function createHistorySearch({ readBody, setTimer = globalThis.setTimeout, clearTimer = globalThis.clearTimeout, timeoutMs = 10_000 } = {}) {
  if (typeof readBody !== "function" || !Number.isSafeInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 60_000) {
    throw new Error("invalid search dependencies or deadline");
  }
  let flight = null;
  let version = 0;
  let dirty = true;
  let state = Object.freeze({ status: "idle", version, criteria: null, hits: Object.freeze([]), error: null });
  const publish = (next) => { state = Object.freeze({ ...next, version }); dirty = true; };
  function retire() {
    version += 1;
    const old = flight;
    flight = null;
    if (old) { clearTimer(old.timer); old.controller.abort(); }
  }
  return {
    async search(input) {
      retire();
      let criteria;
      try { criteria = criteriaFor(input); }
      catch (error) {
        publish({ status: "error", criteria: null, hits: Object.freeze([]), error: error.message });
        return false;
      }
      const batch = { controller: new AbortController(), timer: null };
      flight = batch;
      publish({ status: "loading", criteria, hits: Object.freeze([]), error: null });
      let onAbort;
      const cancelled = new Promise((_, reject) => {
        onAbort = () => reject(new Error("search deadline"));
        batch.controller.signal.addEventListener("abort", onAbort, { once: true });
      });
      batch.timer = setTimer(() => batch.controller.abort(), timeoutMs);
      try {
        const body = await Promise.race([
          Promise.resolve().then(() => {
            if (batch.controller.signal.aborted) throw new Error("search cancelled");
            return readBody(searchPath(criteria), batch.controller.signal);
          }), cancelled,
        ]);
        if (flight !== batch) return false;
        if (batch.controller.signal.aborted) throw new Error("search deadline");
        const hits = decodeResults(body, criteria);
        publish({ status: "ready", criteria, hits, error: null });
        return true;
      } catch (_) {
        if (flight !== batch) return false;
        publish({ status: "error", criteria, hits: Object.freeze([]), error: batch.controller.signal.aborted
          ? "Search timed out. Narrow the query or time range and try again."
          : "Search failed or returned an invalid response. Check the query and server availability." });
        return false;
      } finally {
        clearTimer(batch.timer);
        batch.controller.signal.removeEventListener("abort", onAbort);
        if (flight === batch) flight = null;
      }
    },
    cancel() {
      retire();
      publish({ status: "cancelled", criteria: null, hits: Object.freeze([]), error: null });
    },
    clear() {
      retire();
      publish({ status: "idle", criteria: null, hits: Object.freeze([]), error: null });
    },
    snapshot() { return state; },
    takeUpdate() { if (!dirty) return null; dirty = false; return state; },
    // A button from an obsolete query must not open a different result that
    // subsequently occupies the same row index.
    hit(index, expectedVersion) {
      return state.status === "ready" && state.version === expectedVersion && Number.isSafeInteger(index)
        ? state.hits[index] ?? null : null;
    },
  };
}
