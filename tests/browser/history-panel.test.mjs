import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { mountHistorySearch } from "../../crates/frankenterm-browser-ui/web/history-panel.mjs";

const OUTPUT_EPOCH = "0123456789abcdef0123456789abcdef";
const captureCursor = (id, pane = "7") =>
  `delta-v2:${OUTPUT_EPOCH}:pane:${pane}:${BigInt(id) - 1n}`;

const flush = async () => { for (let n = 0; n < 20; n++) await Promise.resolve(); };
const hit = (id = 3, pane = 7, snippet = "captured needle") => ({ segment_id: id, pane_id: pane,
  score: -0.5, captured_at: 1000, content_len: 24, snippet, capture_cursor: captureCursor(id, pane) });
const envelope = (rows = [hit()]) => JSON.stringify({ ok: true, data: { results: rows, total: rows.length } });
class Node {
  constructor(tag = "div") { this.tag = tag; this.listeners = new Map(); this.children = []; this.attrs = {}; this.value = ""; this.textContent = ""; this.open = false; this.disabled = false; }
  addEventListener(type, callback) { if (!this.listeners.has(type)) this.listeners.set(type, []); this.listeners.get(type).push(callback); }
  emit(type) { const event = { target: this, prevented: false, preventDefault() { this.prevented = true; } }; for (const fn of this.listeners.get(type) ?? []) fn(event); return event; }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  setAttribute(key, value) { this.attrs[key] = value; }
  focus() { this.focused = true; }
  showModal() { this.open = true; }
  close() { this.open = false; this.emit("close"); }
  set innerHTML(_) { throw new Error("captured data must never become HTML"); }
}
function fixture() {
  const nodes = new Map();
  const html = readFileSync(new URL("../../crates/frankenterm-browser-ui/web/index.html", import.meta.url), "utf8");
  for (const [, id] of html.matchAll(/\bid="([^"]+)"/g)) nodes.set(id, new Node());
  const doc = { getElementById: (id) => nodes.get(id), createElement: (tag) => new Node(tag) };
  const calls = [], inspected = [], captures = [];
  const panel = mountHistorySearch({ document: doc, inspectPane: (id) => inspected.push(id),
    inspectCapture: (capture) => captures.push(capture),
    readBody(path, signal) { let resolve; const promise = new Promise((r) => { resolve = r; }); calls.push({ path, signal, resolve }); return promise; } });
  const get = (id) => nodes.get(id);
  get("history-mode").value = "phrase";
  get("history-limit").value = "50";
  const submit = async (query = "needle") => {
    get("history-query").value = query;
    assert.equal(get("history-search-form").emit("submit").prevented, true);
    await flush();
  };
  panel.open();
  return { panel, get, calls, inspected, captures, submit };
}

test("search form submits the real endpoint query and displays ranked matches", async () => {
  const f = fixture();
  assert.equal(f.get("history-search").open, true);
  assert.equal(f.get("history-query").focused, true);
  await f.submit("usage limit");
  assert.equal(f.calls.length, 1);
  const url = new URL(f.calls[0].path, "https://example.test/");
  assert.equal(url.pathname, "/search");
  assert.equal(url.searchParams.get("q"), '"usage limit"');
  assert.equal(f.get("history-search-form").attrs["aria-busy"], "true");
  f.calls[0].resolve(envelope([hit(5), hit(3)]));
  await flush();
  const list = f.get("history-results");
  assert.equal(list.children.length, 2);
  assert.match(list.children[0].children[0].textContent, /Pane 7 · segment 5/);
  assert.match(f.get("history-status").textContent, /2 ranked matches/);
  assert.match(f.get("history-status").textContent, /not a total-match count/);
  assert.equal(f.get("history-search-form").attrs["aria-busy"], "false");
  f.panel.close();
});

test("captured markup is inert text; live inspection binds exact large IDs", async () => {
  const f = fixture();
  await f.submit();
  const text = '<img src=x onerror=alert(1)> & <b>hit</b>';
  const wire = envelope([hit(3, 7, text)])
    .replace('"pane_id":7', '"pane_id":18446744073709551615')
    .replace(captureCursor(3), captureCursor(3, "18446744073709551615"));
  f.calls[0].resolve(wire); await flush();
  const item = f.get("history-results").children[0];
  assert.equal(item.children[1].textContent, text);
  item.children[3].emit("click");
  assert.deepEqual(f.inspected, ["18446744073709551615"]);
  assert.equal(f.get("history-search").open, false);
  assert.equal(f.get("history-results").children.length, 0);
});

test("old result actions cannot open a pane after a replacement query", async () => {
  const f = fixture(); await f.submit();
  f.calls[0].resolve(envelope()); await flush();
  const old = f.get("history-results").children[0].children[3];
  await f.submit("new");
  old.emit("click"); assert.deepEqual(f.inspected, []);
  f.calls[1].resolve(envelope([hit(5, 8)])); await flush();
  old.emit("click"); assert.deepEqual(f.inspected, []);
  f.get("history-results").children[0].children[3].emit("click");
  assert.deepEqual(f.inspected, ["8"]);
});

test("cancel, Escape, and page suspension settle requests and clear retained data", async () => {
  for (const action of ["cancel", "escape", "suspend"]) {
    const f = fixture(); await f.submit("private query");
    if (action === "cancel") f.get("history-cancel").emit("click");
    if (action === "escape") assert.equal(f.get("history-search").emit("cancel").prevented, true);
    if (action === "suspend") f.panel.suspend();
    assert.equal(f.calls[0].signal.aborted, true);
    f.calls[0].resolve(envelope()); await flush();
    assert.equal(f.get("history-results").children.length, 0);
    if (action !== "cancel") assert.equal(f.get("history-query").value, "");
    f.panel.close();
  }
});

test("native datetime-local filters are sent as local epoch milliseconds", async () => {
  const f = fixture();
  f.get("history-since").value = "2026-10-01T08:30";
  f.get("history-until").value = "2026-10-08T08:30";
  f.get("history-pane").value = "007";
  f.get("history-mode").value = "fts";
  f.get("history-limit").value = "25";
  await f.submit("panic OR clippy");
  const params = new URL(f.calls[0].path, "https://example.test/").searchParams;
  assert.equal(params.get("since"), String(new Date("2026-10-01T08:30").getTime()));
  assert.equal(params.get("until"), String(new Date("2026-10-08T08:30").getTime()));
  assert.equal(params.get("pane_id"), "7");
  assert.equal(params.get("q"), "panic OR clippy");
  assert.equal(params.get("limit"), "25");
  f.panel.close();
});

test("invalid local time cancels an older query without silently broadening scope", async () => {
  const f = fixture(); await f.submit();
  f.get("history-since").value = "not a date";
  await f.submit();
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].signal.aborted, true);
  assert.match(f.get("history-status").textContent, /valid local/);
  f.panel.close();
});

test("no matches and request failures have distinct visible outcomes", async () => {
  const f = fixture(); await f.submit();
  f.calls[0].resolve(envelope([])); await flush();
  assert.match(f.get("history-status").textContent, /0 ranked matches/);
  await f.submit();
  f.calls[1].resolve("PRIVATE proxy diagnostics"); await flush();
  assert.match(f.get("history-status").textContent, /Search failed/);
  assert.doesNotMatch(f.get("history-status").textContent, /PRIVATE/);
  f.panel.close();
});

test("closing and reopening ignores late reads and queued prior-close events", async () => {
  const f = fixture(); await f.submit("old");
  f.panel.close(); f.panel.open(); await f.submit("new");
  f.get("history-search").emit("close"); // queued old native close event
  f.calls[0].resolve(envelope([hit(1)]));
  f.calls[1].resolve(envelope([hit(2)])); await flush();
  assert.equal(f.get("history-results").children.length, 1);
  assert.match(f.get("history-results").children[0].children[0].textContent, /segment 2/);
  f.panel.close();
});

test("page wiring and hashed package include the search controller, panel and styles", () => {
  const root = new URL("../../", import.meta.url);
  const app = readFileSync(new URL("crates/frankenterm-browser-ui/web/app.mjs", root), "utf8");
  const script = readFileSync(new URL("scripts/build-browser-ui.sh", root), "utf8");
  const html = readFileSync(new URL("crates/frankenterm-browser-ui/web/index.html", root), "utf8");
  for (const name of ["history-search.mjs", "history-panel.mjs", "history-panel.css"]) assert.ok(script.includes(`$crate_dir/web/${name}`), name);
  assert.match(app, /mountHistorySearch\(\{/);
  assert.match(app, /readBody:.*readSnapshot\(path, apiRoot, fetchImpl, signal, 8 \* 1024 \* 1024\)/);
  assert.match(app, /historySearch\.suspend\(\)/);
  assert.match(app, /inspector\.fillPaneForm\(paneId\)/);
  assert.match(html, /<dialog id="history-search"/);
  const ids = [...html.matchAll(/\bid="([^"]+)"/g)].map((m) => m[1]);
  assert.equal(new Set(ids).size, ids.length);
});


test("historical inspection keeps exact segment, pane, time and stored length without opening current output", async () => {
  const f = fixture();
  await f.submit();
  const wire = envelope([hit()])
    .replace('"segment_id":3', '"segment_id":9223372036854775807')
    .replace('"pane_id":7', '"pane_id":18446744073709551615')
    .replace('"captured_at":1000', '"captured_at":9007199254740993')
    .replace('"content_len":24', '"content_len":9007199254740993')
    .replace(captureCursor(3), captureCursor("9223372036854775807", "18446744073709551615"));
  f.calls[0].resolve(wire); await flush();
  const item = f.get("history-results").children[0];
  assert.equal(item.children[2].textContent, "Open captured segment");
  assert.match(item.children[3].textContent, /Open current output/);
  item.children[2].emit("click");
  assert.deepEqual(f.captures, [{
    segmentId: "9223372036854775807", paneId: "18446744073709551615",
    capturedAt: "9007199254740993", contentLength: "9007199254740993",
    captureCursor: captureCursor("9223372036854775807", "18446744073709551615"),
  }]);
  assert.deepEqual(f.inspected, []);
  assert.equal(f.get("history-search").open, false);
  assert.equal(f.get("history-results").children.length, 0);
});

test("missing or mismatched capture provenance exposes no historical navigation action", async () => {
  for (const cursor of [undefined, captureCursor(3, 8), captureCursor(4)]) {
    const f = fixture(); await f.submit();
    f.calls[0].resolve(envelope([{ ...hit(), capture_cursor: cursor }]));
    await flush();
    assert.equal(f.get("history-results").children.length, 0);
    assert.match(f.get("history-status").textContent, /Search failed/);
    assert.deepEqual(f.captures, []);
    assert.deepEqual(f.inspected, []);
    f.panel.close();
  }
});

test("historical actions from prior queries or closed dialogs cannot open another capture", async () => {
  const f = fixture(); await f.submit();
  f.calls[0].resolve(envelope()); await flush();
  const old = f.get("history-results").children[0].children[2];
  await f.submit("new");
  old.emit("click"); assert.deepEqual(f.captures, []);
  f.calls[1].resolve(envelope([hit(5, 8)])); await flush();
  old.emit("click"); assert.deepEqual(f.captures, []);
  const current = f.get("history-results").children[0].children[2];
  current.emit("click");
  assert.deepEqual(f.captures, [{ segmentId: "5", paneId: "8", capturedAt: "1000", contentLength: "24", captureCursor: captureCursor(5, 8) }]);
  current.emit("click"); old.emit("click");
  assert.equal(f.captures.length, 1);
  assert.deepEqual(f.inspected, []);
});

test("opening a capture preserves the search for returning to another ranked result", async () => {
  const f = fixture();
  f.get("history-pane").value = "7";
  f.get("history-limit").value = "25";
  await f.submit("two retained errors");
  f.calls[0].resolve(envelope([hit(8), hit(3)])); await flush();
  const firstAction = f.get("history-results").children[0].children[2];
  firstAction.emit("click");
  assert.deepEqual(f.captures, [{ segmentId: "8", paneId: "7", capturedAt: "1000", contentLength: "24", captureCursor: captureCursor(8) }]);
  assert.equal(f.get("history-search").open, false);
  assert.equal(f.get("history-results").children.length, 0, "hide the rendered capture text during navigation");
  assert.equal(f.get("history-search-open").textContent, "Return to search results");
  firstAction.emit("click");
  assert.equal(f.captures.length, 1, "hidden result actions cannot navigate again");

  f.panel.open();
  assert.equal(f.get("history-query").value, "two retained errors");
  assert.equal(f.get("history-pane").value, "7");
  assert.equal(f.get("history-limit").value, "25");
  assert.equal(f.calls.length, 1, "returning does not repeat or reorder the search");
  assert.equal(f.get("history-results").children.length, 2);
  firstAction.emit("click");
  assert.equal(f.captures.length, 1, "a previous dialog view cannot reuse a detached result action");
  f.get("history-results").children[1].children[2].emit("click");
  assert.deepEqual(f.captures[1], { segmentId: "3", paneId: "7", capturedAt: "1000", contentLength: "24", captureCursor: captureCursor(3) });
  f.panel.close();
});

test("current-output navigation also retains search, while explicit close clears it", async () => {
  const f = fixture(); await f.submit("retained query");
  f.calls[0].resolve(envelope([hit(6)])); await flush();
  f.get("history-results").children[0].children[3].emit("click");
  assert.deepEqual(f.inspected, ["7"]);
  f.get("history-search-open").emit("click");
  assert.equal(f.get("history-query").value, "retained query");
  assert.equal(f.get("history-results").children.length, 1);
  assert.equal(f.calls.length, 1);
  f.get("history-close").emit("click");
  f.panel.open();
  assert.equal(f.get("history-query").value, "");
  assert.equal(f.get("history-results").children.length, 0);
  f.panel.close();
});

test("suspension, Escape and native close purge retained navigation state and stale actions", async () => {
  for (const finish of ["suspend", "escape", "native-close"]) {
    const f = fixture(); await f.submit("private retained query");
    f.calls[0].resolve(envelope([hit(6)])); await flush();
    const action = f.get("history-results").children[0].children[2];
    action.emit("click");
    if (finish === "suspend") f.panel.suspend();
    else {
      f.panel.open();
      if (finish === "escape") f.get("history-search").emit("cancel");
      else f.get("history-search").close();
    }
    f.panel.open();
    assert.equal(f.get("history-query").value, "");
    assert.equal(f.get("history-results").children.length, 0);
    action.emit("click");
    assert.equal(f.captures.length, 1);
    f.panel.close();
  }
});

test("queued navigation-close events cannot clear reopened results or a replacement search", async () => {
  const f = fixture(); await f.submit("old");
  f.calls[0].resolve(envelope([hit(1)])); await flush();
  const old = f.get("history-results").children[0].children[2];
  old.emit("click");
  f.panel.open();
  f.get("history-search").emit("close");
  assert.equal(f.get("history-results").children.length, 1);
  await f.submit("new");
  f.get("history-search").emit("close");
  old.emit("click");
  assert.equal(f.captures.length, 1);
  f.calls[1].resolve(envelope([hit(2)])); await flush();
  assert.match(f.get("history-results").children[0].children[0].textContent, /segment 2/);
  f.panel.close();
});

const saved = (overrides = {}) => ({ id: "saved-errors", name: "Build failures", query: "error OR panic",
  pane_id: 7, limit: 37, since_mode: "fixed", since_ms: 1001, enabled: true, ...overrides });
const bookmark = (overrides = {}) => ({ pane_id: 7, alias: "build", tags: ["prod"], description: "Build host", ...overrides });
const collection = (key, rows) => JSON.stringify({ ok: true, data: { [key]: rows, total: rows.length } });
async function loadItems(f, searches = [saved()], bookmarks = [bookmark()]) {
  const first = f.calls.length;
  f.get("history-library-load").emit("click"); await flush();
  assert.deepEqual(f.calls.slice(first).map((call) => call.path), ["saved-searches", "bookmarks"]);
  f.calls[first].resolve(typeof searches === "string" ? searches : collection("saved_searches", searches));
  f.calls[first + 1].resolve(typeof bookmarks === "string" ? bookmarks : collection("bookmarks", bookmarks));
  await flush();
}

test("configured saved search fills raw FTS, exact pane, precise start and non-preset limit for the real search", async () => {
  const f = fixture();
  const start = Date.parse("2026-11-01T06:30:00.123Z");
  const wire = collection("saved_searches", [saved({ since_ms: start })])
    .replace('"pane_id":7', '"pane_id":18446744073709551615');
  await loadItems(f, wire);
  f.get("history-until").value = "2026-01-01T01:00";
  f.get("history-saved-list").children[0].children[2].emit("click");
  assert.equal(f.get("history-query").value, "error OR panic");
  assert.equal(f.get("history-mode").value, "fts");
  assert.equal(f.get("history-pane").value, "18446744073709551615");
  assert.equal(f.get("history-limit").value, "37");
  assert.equal(f.get("history-until").value, "", "a previous query's end boundary must not narrow the saved search");
  assert.match(f.get("history-loaded-search").textContent, /fixed start: 2026-11-01T06:30:00.123Z/);
  assert.match(f.get("history-loaded-search").textContent, /does not update/);
  f.get("history-search-form").emit("submit"); await flush();
  const params = new URL(f.calls[2].path, "https://example.test/").searchParams;
  assert.equal(params.get("q"), "error OR panic");
  assert.equal(params.get("pane_id"), "18446744073709551615");
  assert.equal(params.get("since"), String(start));
  assert.equal(params.get("until"), null);
  assert.equal(params.get("limit"), "37");
  f.panel.close();
});

test("last-run searches use stored scheduler time; editing the date replaces that boundary", async () => {
  const f = fixture();
  await loadItems(f, [saved({ since_mode: "last_run", since_ms: 9999, last_run_at: 2002, enabled: false })]);
  const item = f.get("history-saved-list").children[0];
  assert.match(item.children[3].textContent, /Schedule disabled/);
  item.children[2].emit("click");
  f.get("history-search-form").emit("submit"); await flush();
  assert.equal(new URL(f.calls[2].path, "https://example.test/").searchParams.get("since"), "2002");
  f.calls[2].resolve(envelope([])); await flush();
  f.get("history-since").value = "2026-11-10T12:01:02.003";
  f.get("history-since").emit("input");
  f.get("history-search-form").emit("submit"); await flush();
  assert.equal(new URL(f.calls[3].path, "https://example.test/").searchParams.get("since"), String(new Date("2026-11-10T12:01:02.003").getTime()));
  f.panel.close();
});

test("saved settings beyond viewer bounds stay visible without an actionable broadened search", async () => {
  const f = fixture();
  await loadItems(f, [saved({ since_mode: "unknown" }), saved({ id: "large", limit: 201 }),
    saved({ id: "future", since_ms: 9_000_000_000_000_000 })]);
  assert.equal(f.get("history-saved-list").children.length, 3);
  for (const item of f.get("history-saved-list").children) {
    assert.equal(item.children[2].disabled, true);
    item.children[2].emit("click");
  }
  assert.equal(f.get("history-query").value, "");
  assert.equal(f.calls.length, 2);
  assert.match(f.get("history-saved-list").children[2].children[3].textContent, /date range/);
  f.panel.close();
});

test("saved labels, queries and bookmark descriptions remain inert text and filtering preserves exact actions", async () => {
  const f = fixture();
  const markup = '<img src=x onerror=alert(1)>';
  const wire = collection("bookmarks", [bookmark({ alias: markup, description: "production <b>host</b>" }),
    bookmark({ alias: "second", tags: ["staging"], pane_id: 8 })])
    .replace('"pane_id":7', '"pane_id":18446744073709551615');
  await loadItems(f, [saved({ name: markup, query: "<script>alert(1)</script>" })], wire);
  assert.match(f.get("history-saved-list").children[0].children[0].textContent, /<img/);
  assert.equal(f.get("history-saved-list").children[0].children[1].textContent, "<script>alert(1)</script>");
  const old = f.get("history-bookmark-list").children[0].children[2];
  f.get("history-library-filter").value = "PROD";
  f.get("history-library-filter").emit("input");
  assert.equal(f.get("history-bookmark-list").children.length, 1);
  assert.equal(f.get("history-saved-list").children.length, 0);
  old.emit("click"); assert.deepEqual(f.inspected, []);
  const current = f.get("history-bookmark-list").children[0];
  assert.match(current.children[1].textContent, /<b>host<\/b>/);
  assert.equal(current.children[2].textContent, "Open captured output");
  assert.match(current.children[2].title, /another session/);
  current.children[2].emit("click");
  assert.deepEqual(f.inspected, ["18446744073709551615"]);
  assert.deepEqual(f.captures, []);
  assert.equal(f.get("history-search").open, false);
  current.children[2].emit("click"); assert.equal(f.inspected.length, 1);
  f.panel.open();
  assert.equal(f.calls.length, 2, "returning does not reload saved items");
  assert.equal(f.get("history-bookmark-list").children.length, 1);
  f.panel.close();
});

test("one saved endpoint can fail while the other remains useful, and empty lists are distinct", async () => {
  const f = fixture();
  await loadItems(f, "private backend response", [bookmark()]);
  assert.match(f.get("history-saved-status").textContent, /could not be loaded/);
  assert.doesNotMatch(f.get("history-saved-status").textContent, /private/);
  assert.equal(f.get("history-bookmark-list").children.length, 1);
  await loadItems(f, [], []);
  assert.equal(f.get("history-saved-list").children.length, 0);
  assert.match(f.get("history-saved-status").textContent, /0 of 0/);
  assert.match(f.get("history-bookmark-status").textContent, /0 of 0/);
  f.panel.close();
});

test("refresh and session suspension cannot retarget saved-item buttons or republish late private metadata", async () => {
  const f = fixture(); await loadItems(f);
  const oldSearch = f.get("history-saved-list").children[0].children[2];
  const oldBookmark = f.get("history-bookmark-list").children[0].children[2];
  f.get("history-library-load").emit("click"); await flush();
  oldSearch.emit("click"); oldBookmark.emit("click");
  assert.equal(f.get("history-query").value, "");
  assert.deepEqual(f.inspected, []);
  f.get("history-library-filter").value = "private";
  f.panel.suspend();
  assert.equal(f.calls[2].signal.aborted, true);
  assert.equal(f.calls[3].signal.aborted, true);
  f.panel.open();
  f.calls[2].resolve(collection("saved_searches", [saved({ name: "private" })]));
  f.calls[3].resolve(collection("bookmarks", [bookmark({ alias: "private" })])); await flush();
  assert.equal(f.get("history-saved-list").children.length, 0);
  assert.equal(f.get("history-bookmark-list").children.length, 0);
  assert.equal(f.get("history-library-filter").value, "");
  assert.equal(f.get("history-loaded-search").textContent, "");
  oldSearch.emit("click"); oldBookmark.emit("click");
  assert.equal(f.get("history-query").value, "");
  assert.deepEqual(f.inspected, []);
  f.panel.close();
});

test("cancel saved loading and Escape clear outstanding reads without clearing unrelated search through cancellation", async () => {
  const f = fixture(); await f.submit("manual query");
  f.calls[0].resolve(envelope()); await flush();
  f.get("history-library-load").emit("click"); await flush();
  f.get("history-library-cancel").emit("click");
  assert.equal(f.calls[1].signal.aborted, true);
  assert.equal(f.calls[2].signal.aborted, true);
  assert.match(f.get("history-library-status").textContent, /cancelled/);
  assert.equal(f.get("history-results").children.length, 1);
  f.get("history-search").emit("cancel");
  f.calls[1].resolve(collection("saved_searches", [saved()]));
  f.calls[2].resolve(collection("bookmarks", [bookmark()])); await flush();
  assert.equal(f.get("history-query").value, "");
  assert.equal(f.get("history-results").children.length, 0);
  assert.equal(f.get("history-bookmark-list").children.length, 0);
});
