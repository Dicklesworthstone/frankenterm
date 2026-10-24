// Search retained capture through the existing read-only /search API.
// Results identify database segments, never authority to control a live pane.
const U64_MAX = 18_446_744_073_709_551_615n;
const I64_MAX = 9_223_372_036_854_775_807n;
const MAX_BODY_CHARS = 8 * 1024 * 1024;
const MAX_SNIPPET_CHARS = 16 * 1024;

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

function decodeResults(body, criteria) {
  if (typeof body !== "string" || body.length > MAX_BODY_CHARS) throw new Error("invalid response");
  const value = JSON.parse(body);
  // Preserve exact integer identities but retain ORIGINAL JSON types for
  // validation. Numeric snippet/error fields must not turn into accepted text.
  const exact = JSON.parse(body.replace(
    /"(?:[^"\\]|\\.)*"|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/g,
    (token) => /^-?\d+$/.test(token) ? JSON.stringify(token) : token,
  ));
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
