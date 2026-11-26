import test from "node:test";
import assert from "node:assert/strict";
import { createHistoryLibrary, createHistorySearch } from "../../crates/frankenterm-browser-ui/web/history-search.mjs";

const OUTPUT_EPOCH = "0123456789abcdef0123456789abcdef";
const captureCursor = (id, pane = "7", epoch = OUTPUT_EPOCH) =>
  `delta-v2:${epoch}:pane:${pane}:${BigInt(id) - 1n}`;

const flush = async () => { for (let n = 0; n < 8; n++) await Promise.resolve(); };
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
function hit(segment_id = 3, overrides = {}) {
  return { segment_id, pane_id: 7, score: -0.5, captured_at: 1000, content_len: 24,
    snippet: "synthetic needle", capture_cursor: captureCursor(segment_id, overrides.pane_id ?? 7), ...overrides };
}
const envelope = (rows = [hit()]) => JSON.stringify({ ok: true, data: { results: rows, total: rows.length } });
function fixture() {
  let next = 0;
  const timers = new Map();
  const calls = [];
  const search = createHistorySearch({
    readBody(path, signal) {
      const reply = deferred();
      calls.push({ url: new URL(path, "http://localhost/"), signal, ...reply });
      return reply.promise;
    },
    setTimer(fn, ms) { const id = ++next; timers.set(id, { fn, ms }); return id; },
    clearTimer(id) { timers.delete(id); },
  });
  return { search, calls, timers };
}
async function answer(input, body) {
  const f = fixture();
  const pending = f.search.search(input);
  await flush();
  assert.equal(f.calls.length, 1);
  f.calls[0].resolve(body);
  const ok = await pending;
  assert.equal(f.timers.size, 0, "completed request releases its deadline");
  return { ...f, ok };
}

test("phrase search uses the real API filters and explicit FTS quoting", async () => {
  const { calls, search, ok } = await answer({ query: 'NOT "panic" & error', paneId: "007", sinceMs: "900", untilMs: "1100", limit: 20 }, envelope());
  assert.equal(ok, true);
  const params = calls[0].url.searchParams;
  assert.equal(calls[0].url.pathname, "/search");
  assert.equal(params.get("q"), '"NOT ""panic"" & error"');
  assert.equal(params.get("pane_id"), "7");
  assert.equal(params.get("since"), "900");
  assert.equal(params.get("until"), "1100");
  assert.equal(params.get("limit"), "20");
  assert.equal(search.snapshot().hits[0].segmentId, "3");
  assert.equal(search.snapshot().hits[0].captureCursor, captureCursor(3));
  assert.equal(search.snapshot().criteria.mode, "phrase");
});

test("raw FTS is opt-in; empty filters are omitted, not coerced to zero", async () => {
  const { calls, ok } = await answer({ query: 'panic OR "usage limit"', mode: "fts" }, envelope([]));
  assert.equal(ok, true);
  assert.equal(calls[0].url.searchParams.get("q"), 'panic OR "usage limit"');
  for (const key of ["pane_id", "since", "until", "offset"]) assert.equal(calls[0].url.searchParams.has(key), false);
});

test("exact i64/u64 identities survive JSON numbers beyond 2^53", async () => {
  // Construct the wire text explicitly; JSON.stringify(Number) would round it.
  const wire = `{"ok":true,"data":{"total":1,"results":[{"segment_id":9223372036854775807,"pane_id":18446744073709551615,"captured_at":9007199254740993,"content_len":9007199254740995,"score":0,"snippet":"literal 123","capture_cursor":"${captureCursor("9223372036854775807", "18446744073709551615")}"}]}}`;
  const { search, ok, calls } = await answer({ query: "literal", paneId: "18446744073709551615", sinceMs: "9007199254740993" }, wire);
  assert.equal(ok, true);
  assert.equal(calls[0].url.searchParams.get("pane_id"), "18446744073709551615");
  assert.deepEqual(search.snapshot().hits[0], {
    segmentId: "9223372036854775807", paneId: "18446744073709551615", capturedAt: "9007199254740993",
    contentLength: "9007199254740995",
    captureCursor: captureCursor("9223372036854775807", "18446744073709551615"),
    score: 0, snippet: "literal 123",
  });
});

test("capture provenance is required and cannot retarget a result or admit a partial prefix", async () => {
  const invalid = [
    undefined, null, "", 123, "delta-v1:pane:7:2",
    `delta-v2:${OUTPUT_EPOCH}:all:2`,
    captureCursor(3, 8), captureCursor(2), captureCursor(4),
    captureCursor(3).replace(":pane:7:", ":pane:07:"),
    captureCursor(3).replace(/:2$/, ":02"),
    captureCursor(3, 7, OUTPUT_EPOCH.toUpperCase()),
    captureCursor(3) + "\0", captureCursor(3) + "\r\n",
    `delta-v2:${OUTPUT_EPOCH}:pane:7:9223372036854775808`,
    "x".repeat(129),
  ];
  for (const cursor of invalid) {
    const { search, ok } = await answer({ query: "needle" },
      envelope([hit(4), hit(3, { capture_cursor: cursor })]));
    assert.equal(ok, false);
    assert.equal(search.snapshot().status, "error");
    assert.equal(search.snapshot().hits.length, 0, "no actionable accepted prefix");
  }
  const { search, ok } = await answer({ query: "needle" }, envelope([hit(1)]));
  assert.equal(ok, true);
  assert.equal(search.snapshot().hits[0].captureCursor, captureCursor(1));
  assert.throws(() => { search.snapshot().hits[0].captureCursor = captureCursor(2); }, TypeError);
});

test("invalid input cancels prior work and never reaches the reader", async () => {
  const invalid = [
    {}, { query: " " }, { query: "a".repeat(2049) }, { query: "a\0b" },
    { query: "a", mode: "unknown" }, { query: "a", paneId: "-1" },
    { query: "a", paneId: "18446744073709551616" }, { query: "a", paneId: 7 },
    { query: "a", sinceMs: "1e3" }, { query: "a", untilMs: "9223372036854775808" },
    { query: "a", sinceMs: "12", untilMs: "11" }, { query: "a", limit: 201 }, { query: "a", limit: 0.5 },
  ];
  for (const input of invalid) {
    const { search, calls, timers } = fixture();
    assert.equal(await search.search(input), false);
    assert.equal(calls.length, 0);
    assert.equal(timers.size, 0);
    assert.equal(search.snapshot().status, "error");
    assert.equal(search.snapshot().hits.length, 0);
  }
});

test("malformed, oversized, wrong-scope and duplicate results fail atomically", async () => {
  const invalidBodies = [
    "<html>private proxy failure</html>", "null", "[]", "{1:2}",
    JSON.stringify({ ok: false, error: "private backend detail", data: { results: [], total: 0 } }),
    JSON.stringify({ ok: true, data: { results: [hit()], total: 999 } }),
    envelope([hit(), hit()]), envelope([hit(0)]), envelope([hit(1, { pane_id: 8 })]),
    envelope([hit(1, { captured_at: 899 })]), envelope([hit(1, { captured_at: 1101 })]),
    envelope([hit(1, { snippet: 123 })]), envelope([hit(1, { score: null })]),
    envelope([hit(1, { content_len: -1 })]), envelope([hit(1, { snippet: "x".repeat(16 * 1024 + 1) })]),
    envelope([hit(1)]).replace('"segment_id":1', '"segment_id":1e0'),
    envelope([hit(1)]).replace('"segment_id":1', '"segment_id":"1"'),
    envelope([hit(1)]).replace('"segment_id":1', '"segment_id":9223372036854775808'),
    " ".repeat(8 * 1024 * 1024 + 1),
  ];
  for (const body of invalidBodies) {
    const { search, ok } = await answer({ query: "x", paneId: "7", sinceMs: "900", untilMs: "1100" }, body);
    assert.equal(ok, false);
    assert.equal(search.snapshot().status, "error");
    assert.equal(search.snapshot().hits.length, 0);
    assert.equal(search.snapshot().error.includes("private"), false);
  }
});

test("response count is bounded; missing snippets and unknown fields remain usable", async () => {
  const valid = await answer({ query: "x", limit: 2 }, envelope([hit(4, { snippet: null, future: true }), hit(3)]));
  assert.equal(valid.ok, true);
  assert.deepEqual(valid.search.snapshot().hits.map((row) => row.segmentId), ["4", "3"], "preserve server relevance order");
  assert.equal(valid.search.snapshot().hits[0].snippet, "");
  const invalid = await answer({ query: "x", limit: 1 }, envelope([hit(4), hit(3)]));
  assert.equal(invalid.ok, false);
});

test("replacing a search aborts it and ignores its later successful response", async () => {
  const { search, calls, timers } = fixture();
  const first = search.search({ query: "old" });
  await flush();
  const second = search.search({ query: "new" });
  await flush();
  assert.equal(calls[0].signal.aborted, true);
  assert.equal(timers.size, 1);
  calls[1].resolve(envelope([hit(22)]));
  assert.equal(await second, true);
  calls[0].resolve(envelope([hit(11)]));
  assert.equal(await first, false);
  await flush();
  assert.equal(search.snapshot().criteria.query, "new");
  assert.equal(search.snapshot().hits[0].segmentId, "22");
  assert.equal(timers.size, 0);
});

test("deadline bounds even a reader that never resolves or honors abort", async () => {
  const { search, calls, timers } = fixture();
  const pending = search.search({ query: "x" });
  await flush();
  const timer = [...timers.values()][0];
  assert.equal(timer.ms, 10000);
  timer.fn();
  assert.equal(await pending, false);
  assert.equal(calls[0].signal.aborted, true);
  assert.match(search.snapshot().error, /timed out/);
  assert.equal(timers.size, 0);
});

test("cancel/clear retire stale actions and settle outstanding searches", async () => {
  const { search, calls, timers } = fixture();
  const pending = search.search({ query: "x" });
  await flush();
  search.cancel();
  assert.equal(await pending, false);
  assert.equal(calls[0].signal.aborted, true);
  assert.equal(search.snapshot().status, "cancelled");
  assert.equal(search.snapshot().criteria, null);
  search.clear();
  assert.equal(search.snapshot().status, "idle");
  assert.equal(timers.size, 0);
  calls[0].reject(new Error("late private details"));
  await flush();
  assert.equal(search.snapshot().status, "idle");
});

test("result buttons are version-bound and cannot be retargeted by a new query", async () => {
  const { search } = await answer({ query: "x" }, envelope());
  const old = search.snapshot();
  assert.equal(search.hit(0, old.version).segmentId, "3");
  assert.equal(search.hit(-1, old.version), null);
  assert.equal(search.hit(0.5, old.version), null);
  assert.throws(() => { old.hits[0].paneId = "99"; }, TypeError);
  search.clear();
  assert.equal(search.hit(0, old.version), null);
  assert.equal(search.takeUpdate().status, "idle");
  assert.equal(search.takeUpdate(), null);
});

test("synchronous reader failure is finite and releases its timer", async () => {
  const search = createHistorySearch({ readBody() { throw new Error("private token must not leak"); } });
  assert.equal(await search.search({ query: "x" }), false);
  assert.equal(search.snapshot().error.includes("private"), false);
});

const saved = (overrides = {}) => ({ id: "saved-errors", name: "Build failures", query: "error OR panic",
  pane_id: 7, limit: 37, since_mode: "fixed", since_ms: 1001, enabled: true, ...overrides });
const bookmark = (overrides = {}) => ({ pane_id: 7, alias: "build", tags: ["prod"], description: "Build host", ...overrides });
const collection = (key, rows) => JSON.stringify({ ok: true, data: { [key]: rows, total: rows.length } });
function libraryFixture() {
  let next = 0;
  const timers = new Map(), calls = [];
  const library = createHistoryLibrary({
    readBody(path, signal) { const reply = deferred(); calls.push({ path, signal, ...reply }); return reply.promise; },
    setTimer(fn, ms) { const id = ++next; timers.set(id, { fn, ms }); return id; },
    clearTimer(id) { timers.delete(id); },
  });
  return { library, calls, timers };
}
async function loadLibrary(savedBody = collection("saved_searches", [saved()]), bookmarkBody = collection("bookmarks", [bookmark()])) {
  const f = libraryFixture();
  const pending = f.library.load();
  await flush();
  assert.deepEqual(f.calls.map((call) => call.path), ["saved-searches", "bookmarks"]);
  f.calls[0].resolve(savedBody); f.calls[1].resolve(bookmarkBody);
  const ok = await pending;
  assert.equal(f.timers.size, 0);
  return { ...f, ok };
}

test("saved items retain exact pane identities and the real fixed or last-run search boundary", async () => {
  const wire = collection("saved_searches", [saved(), saved({ id: "last", since_mode: "last_run", since_ms: 9999, last_run_at: 2002 }),
    saved({ id: "never", since_mode: "last_run", pane_id: null, last_run_at: null })])
    .replace('"pane_id":7', '"pane_id":18446744073709551615');
  const bookmarks = collection("bookmarks", [bookmark()]).replace('"pane_id":7', '"pane_id":9007199254740993');
  const f = await loadLibrary(wire, bookmarks);
  assert.equal(f.ok, true);
  const state = f.library.snapshot();
  assert.equal(state.savedSearches.items[0].paneId, "18446744073709551615");
  assert.equal(state.savedSearches.items[0].limit, "37");
  assert.equal(state.savedSearches.items[0].sinceMs, "1001");
  assert.equal(state.savedSearches.items[1].sinceMs, "2002", "last_run takes precedence over since_ms");
  assert.equal(state.savedSearches.items[2].sinceMs, null);
  assert.equal(state.savedSearches.items[2].paneId, null);
  assert.equal(state.bookmarks.items[0].paneId, "9007199254740993");
  assert.throws(() => { state.bookmarks.items[0].tags.push("changed"); }, TypeError);
  assert.equal(f.library.savedSearch(0, state.version).query, "error OR panic");
  assert.equal(f.library.bookmark(-1, state.version), null);
  assert.equal(f.library.savedSearch(0.5, state.version), null);
});

test("unsupported saved settings are retained with an explicit refusal instead of broadening scope", async () => {
  const f = await loadLibrary(collection("saved_searches", [
    saved({ since_mode: "relative" }), saved({ id: "large", limit: 201 }),
    saved({ id: "manual", enabled: false }),
  ]));
  assert.equal(f.ok, true);
  assert.match(f.library.snapshot().savedSearches.items[0].unavailable, /time mode/);
  assert.match(f.library.snapshot().savedSearches.items[1].unavailable, /201/);
  assert.equal(f.library.snapshot().savedSearches.items[2].unavailable, null, "disabled schedules can still be searched manually");
});

test("invalid saved-search responses fail as a whole while bookmarks remain usable", async () => {
  const invalid = ["private upstream diagnostics", "null", collection("bookmarks", []),
    collection("saved_searches", [saved(), saved()]),
    collection("saved_searches", [saved({ query: 123 })]),
    collection("saved_searches", [saved({ query: "x".repeat(2049) })]),
    collection("saved_searches", [saved({ name: "" })]),
    collection("saved_searches", [saved({ limit: 0 })]),
    collection("saved_searches", [saved({ enabled: "true" })]),
    collection("saved_searches", [saved({ since_ms: -1 })]),
    collection("saved_searches", [saved({ last_run_at: "1000" })]),
    collection("saved_searches", [saved()]).replace('"pane_id":7', '"pane_id":7e0'),
    collection("saved_searches", [saved()]).replace('"pane_id":7', '"pane_id":18446744073709551616'),
    collection("saved_searches", [saved()]).replace('"total":1', '"total":2'),
    collection("saved_searches", Array.from({ length: 501 }, (_, n) => saved({ id: String(n) }))),
    " ".repeat(8 * 1024 * 1024 + 1),
  ];
  for (const body of invalid) {
    const f = await loadLibrary(body);
    assert.equal(f.ok, false);
    assert.equal(f.library.snapshot().savedSearches.status, "error");
    assert.equal(f.library.snapshot().savedSearches.items.length, 0);
    assert.doesNotMatch(f.library.snapshot().savedSearches.error, /private/);
    assert.equal(f.library.snapshot().bookmarks.items[0].paneId, "7");
  }
});

test("malformed bookmarks cannot become actionable numeric identities or an accepted prefix", async () => {
  for (const rows of [[bookmark(), bookmark()], [bookmark({ pane_id: "7" })], [bookmark({ pane_id: 1.5 })],
    [bookmark({ tags: "prod" })], [bookmark({ tags: Array(65).fill("prod") })], [bookmark({ tags: [123] })],
    [bookmark({ description: 123 })], [bookmark({ description: "x".repeat(4097) })], [bookmark({ alias: "\0bad" })]]) {
    const f = await loadLibrary(collection("saved_searches", [saved()]), collection("bookmarks", rows));
    assert.equal(f.ok, false);
    assert.equal(f.library.snapshot().bookmarks.items.length, 0);
    assert.equal(f.library.snapshot().bookmarks.status, "error");
    assert.equal(f.library.snapshot().savedSearches.items.length, 1);
  }
});

test("a saved-items deadline preserves the answered endpoint and bounds an uncooperative reader", async () => {
  const f = libraryFixture();
  const pending = f.library.load(); await flush();
  f.calls[0].resolve(collection("saved_searches", [saved()])); await flush();
  const timer = [...f.timers.values()][0];
  assert.equal(timer.ms, 10000);
  timer.fn();
  assert.equal(await pending, false);
  assert.equal(f.calls[1].signal.aborted, true);
  assert.equal(f.library.snapshot().savedSearches.items.length, 1);
  assert.match(f.library.snapshot().bookmarks.error, /timed out/);
  assert.equal(f.timers.size, 0);
});

test("refresh, cancellation and session clearing retire saved actions and late response callbacks", async () => {
  const f = await loadLibrary();
  const oldVersion = f.library.snapshot().version;
  const first = f.library.load(); await flush();
  const second = f.library.load(); await flush();
  assert.equal(f.calls[2].signal.aborted, true);
  assert.equal(await first, false);
  assert.equal(f.library.savedSearch(0, oldVersion), null);
  f.calls[4].resolve(collection("saved_searches", [saved({ id: "new" })]));
  f.calls[5].resolve(collection("bookmarks", []));
  assert.equal(await second, true);
  f.calls[2].resolve(collection("saved_searches", [saved({ id: "old" })]));
  f.calls[3].reject(new Error("late private failure")); await flush();
  assert.equal(f.library.snapshot().savedSearches.items[0].id, "new");
  const pending = f.library.load(); await flush();
  f.library.cancel();
  assert.equal(await pending, false);
  assert.equal(f.library.snapshot().status, "cancelled");
  assert.equal(f.library.snapshot().savedSearches.items.length, 0);
  f.library.clear();
  f.calls[6].resolve(collection("saved_searches", [saved()]));
  f.calls[7].resolve(collection("bookmarks", [bookmark()])); await flush();
  assert.equal(f.library.snapshot().status, "idle");
  assert.equal(f.timers.size, 0);
});

test("synchronous saved-items failures produce finite errors without leaking backend text", async () => {
  const library = createHistoryLibrary({ readBody() { throw new Error("private credential"); } });
  assert.equal(await library.load(), false);
  assert.doesNotMatch(library.snapshot().savedSearches.error, /private|credential/);
  assert.doesNotMatch(library.snapshot().bookmarks.error, /private|credential/);
});
