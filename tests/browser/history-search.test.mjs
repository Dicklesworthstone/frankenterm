import test from "node:test";
import assert from "node:assert/strict";
import { createHistorySearch } from "../../crates/frankenterm-browser-ui/web/history-search.mjs";

const flush = async () => { for (let n = 0; n < 8; n++) await Promise.resolve(); };
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
function hit(segment_id = 3, overrides = {}) {
  return { segment_id, pane_id: 7, score: -0.5, captured_at: 1000, content_len: 24, snippet: "synthetic needle", ...overrides };
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
  const wire = `{"ok":true,"data":{"total":1,"results":[{"segment_id":9223372036854775807,"pane_id":18446744073709551615,"captured_at":9007199254740993,"content_len":9007199254740995,"score":0,"snippet":"literal 123"}]}}`;
  const { search, ok, calls } = await answer({ query: "literal", paneId: "18446744073709551615", sinceMs: "9007199254740993" }, wire);
  assert.equal(ok, true);
  assert.equal(calls[0].url.searchParams.get("pane_id"), "18446744073709551615");
  assert.deepEqual(search.snapshot().hits[0], {
    segmentId: "9223372036854775807", paneId: "18446744073709551615", capturedAt: "9007199254740993",
    contentLength: "9007199254740995", score: 0, snippet: "literal 123",
  });
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
