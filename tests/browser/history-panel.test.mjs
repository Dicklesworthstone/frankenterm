import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { mountHistorySearch } from "../../crates/frankenterm-browser-ui/web/history-panel.mjs";

const flush = async () => { for (let n = 0; n < 20; n++) await Promise.resolve(); };
const hit = (id = 3, pane = 7, snippet = "captured needle") => ({ segment_id: id, pane_id: pane,
  score: -0.5, captured_at: 1000, content_len: 24, snippet });
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
  const wire = envelope([hit(3, 7, text)]).replace('"pane_id":7', '"pane_id":18446744073709551615');
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
  assert.match(app, /readBody:.*readSnapshot\(path, apiRoot, globalThis.fetch, signal, 8 \* 1024 \* 1024\)/);
  assert.match(app, /historySearch\.suspend\(\)/);
  assert.match(app, /getElementById\("pane-output-id"\)\.value = paneId/);
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
    .replace('"content_len":24', '"content_len":9007199254740993');
  f.calls[0].resolve(wire); await flush();
  const item = f.get("history-results").children[0];
  assert.equal(item.children[2].textContent, "Open captured segment");
  assert.match(item.children[3].textContent, /Open current output/);
  item.children[2].emit("click");
  assert.deepEqual(f.captures, [{
    segmentId: "9223372036854775807", paneId: "18446744073709551615",
    capturedAt: "9007199254740993", contentLength: "9007199254740993",
  }]);
  assert.deepEqual(f.inspected, []);
  assert.equal(f.get("history-search").open, false);
  assert.equal(f.get("history-results").children.length, 0);
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
  assert.deepEqual(f.captures, [{ segmentId: "5", paneId: "8", capturedAt: "1000", contentLength: "24" }]);
  current.emit("click"); old.emit("click");
  assert.equal(f.captures.length, 1);
  assert.deepEqual(f.inspected, []);
});
