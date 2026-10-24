import test from "node:test";
import assert from "node:assert/strict";
import { createPaneOutput, mountPaneOutput } from "../../crates/frankenterm-browser-ui/web/pane-output.mjs";
import { handleFleetKey } from "../../crates/frankenterm-browser-ui/web/app.mjs";
import { readFileSync } from "node:fs";

function fixture(options = {}) {
  const sources = [];
  const timers = new Map();
  let nextTimer = 0;
  class Source {
    static CLOSED = 2;
    constructor(url) {
      this.url = new URL(url);
      this.readyState = 1;
      this.closed = false;
      this.handlers = new Map();
      sources.push(this);
    }
    addEventListener(type, handler) {
      const list = this.handlers.get(type) ?? [];
      list.push(handler);
      this.handlers.set(type, list);
    }
    close() { this.closed = true; this.readyState = Source.CLOSED; }
    emit(type, data, lastEventId = "") {
      for (const handler of this.handlers.get(type) ?? []) handler({ type, data, lastEventId });
    }
    fail(permanent = true) {
      this.readyState = permanent ? Source.CLOSED : 0;
      this.emit("error");
    }
  }
  const output = createPaneOutput({
    apiRoot: new URL("https://example.test/prefix/"), EventSourceImpl: Source,
    setTimer: (fn, delay) => { const id = ++nextTimer; timers.set(id, { fn, delay }); return id; },
    clearTimer: (id) => timers.delete(id), ...options,
  });
  return {
    output, sources, timers,
    latest: () => sources.at(-1),
    retry() {
      assert.equal(timers.size, 1);
      const [id, { fn, delay }] = timers.entries().next().value;
      timers.delete(id); fn(); return delay;
    },
  };
}

function control(source, kind, data = { pane_id: 7 }, inheritedId = "") {
  source.emit(kind, JSON.stringify({ schema: "ft.stream.v1", stream: "deltas", kind, data }), inheritedId);
}

function delta(source, id, content, pane = "7", overrides = {}) {
  const payload = `{"schema":"ft.stream.v1","stream":"deltas","kind":"delta","data":` +
    `{"pane_id":${pane},"segment_id":${id},"content":${JSON.stringify(content)}}}`;
  source.emit("delta", overrides.payload ?? payload, overrides.cursor ?? `delta-v1:pane:${pane}:${id}`);
}

test("selection opens one pane-scoped, read-only live stream", () => {
  const f = fixture();
  f.output.select("7");
  assert.equal(f.sources.length, 1);
  assert.equal(f.latest().url.pathname, "/prefix/stream/deltas");
  assert.equal(f.latest().url.searchParams.get("pane_id"), "7");
  assert.equal(f.latest().url.searchParams.get("max_hz"), "50");
  assert.equal(f.latest().url.searchParams.has("since_id"), false);
  assert.equal(f.output.snapshot().status, "connecting");
  control(f.latest(), "ready");
  assert.equal(f.output.snapshot().status, "connected");
  assert.equal(f.output.snapshot().cursor, null);
});

test("retained history is explicit and resumes from the accepted row", () => {
  const f = fixture();
  f.output.select("7", { retainedHistory: true });
  assert.equal(f.latest().url.searchParams.get("since_id"), "0");
  delta(f.latest(), "12", "old output");
  f.latest().fail();
  assert.equal(f.retry(), 1000);
  assert.equal(f.latest().url.searchParams.get("since_id"), "12");
  assert.equal(f.output.snapshot().text, "old output");
  assert.equal(f.output.snapshot().unanchored, false);
});

test("u64 pane and i64 segment identities never round through Number", () => {
  const f = fixture();
  const pane = "18446744073709551615";
  const id = "9223372036854775807";
  f.output.select(pane);
  delta(f.latest(), id, "exact", pane);
  assert.equal(f.output.snapshot().cursor, id);
  assert.equal(f.output.snapshot().text, "exact");
  f.latest().fail(); f.retry();
  assert.equal(f.latest().url.searchParams.get("pane_id"), pane);
  assert.equal(f.latest().url.searchParams.get("since_id"), id);
});

test("neighboring large numeric identities cannot masquerade as the selected pane", () => {
  const f = fixture();
  f.output.select("9007199254740992");
  delta(f.latest(), "9007199254740993", "wrong", "9007199254740993", {
    cursor: "delta-v1:pane:9007199254740992:9007199254740993",
  });
  assert.equal(f.output.snapshot().text, "");
  assert.equal(f.output.snapshot().cursor, null);
  assert.equal(f.latest().closed, true);
});

test("duplicate and older replay rows cannot append twice or regress the cursor", () => {
  const f = fixture(); f.output.select("7");
  delta(f.latest(), "10", "A"); delta(f.latest(), "10", "duplicate");
  delta(f.latest(), "9", "older"); delta(f.latest(), "13", "B");
  assert.equal(f.output.snapshot().text, "AB");
  assert.equal(f.output.snapshot().cursor, "13");
});

test("control frames preserve received cursor and capture-gap evidence", () => {
  const f = fixture(); f.output.select("7");
  delta(f.latest(), "10", "A");
  control(f.latest(), "gap", { pane_id: 7 }, "delta-v1:pane:7:10");
  control(f.latest(), "lag", { missed_count: 99 });
  control(f.latest(), "ready", { pane_id: 7 });
  const state = f.output.snapshot();
  assert.equal(state.cursor, "10"); assert.equal(state.gapNotifications, 2);
  assert.equal(state.text, "A");
});

test("retiring a pane clears its transcript and ignores old source callbacks", () => {
  const f = fixture(); f.output.select("7"); const old = f.latest();
  delta(old, "1", "private pane seven");
  f.output.select("8");
  assert.equal(old.closed, true);
  delta(old, "2", "late pane seven"); old.fail(); control(old, "gap");
  assert.equal(f.timers.size, 0);
  assert.equal(f.output.snapshot().text, "");
  assert.equal(f.output.snapshot().cursor, null);
  delta(f.latest(), "3", "pane eight", "8");
  assert.equal(f.output.snapshot().text, "pane eight");
});

test("invalid selection has no effect on an existing stream", () => {
  const f = fixture(); f.output.select("7"); const first = f.latest();
  for (const id of ["", "-1", "1.5", "1e3", " 7", "x", "18446744073709551616", 7, null]) {
    assert.throws(() => f.output.select(id), /invalid pane id/);
  }
  assert.equal(f.latest(), first); assert.equal(first.closed, false);
  f.output.select("0007"); assert.equal(f.output.snapshot().paneId, "7");
});

test("bad frame, wrong scope, and overflowing cursor fail closed before admission", () => {
  const cases = [
    { cursor: "1" }, { cursor: "delta-v1:all:2" }, { cursor: "delta-v1:pane:8:2" },
    { cursor: "delta-v1:pane:7:9223372036854775808" }, { cursor: "delta-v1:pane:7:0" },
    { cursor: "delta-v1:pane:7:3" },
    { payload: "not JSON" }, { payload: '{1:2}' },
    { payload: '{"schema":"other","stream":"deltas","kind":"delta","data":{}}' },
    { payload: '{"schema":"ft.stream.v1","stream":"events","kind":"delta","data":{}}' },
    { payload: '{"schema":"ft.stream.v1","stream":"deltas","kind":"ready","data":{}}' },
    { payload: '{"schema":"ft.stream.v1","stream":"deltas","kind":"delta","data":{"pane_id":7,"segment_id":2,"content":42}}' },
  ];
  for (const invalid of cases) {
    const f = fixture(); f.output.select("7");
    delta(f.latest(), "1", "accepted"); delta(f.latest(), "2", "rejected", "7", invalid);
    assert.equal(f.output.snapshot().cursor, "1");
    assert.equal(f.output.snapshot().text, "accepted");
    assert.equal(f.latest().closed, true);
    f.retry(); assert.equal(f.latest().url.searchParams.get("since_id"), "1");
  }
});

test("integer-token decoding leaves quoted, escaped, and Unicode content intact", () => {
  const f = fixture(); f.output.select("7");
  const content = '123 -5 1.2 1e3 "9" \\42 {"pane_id":8} λ😀\n';
  delta(f.latest(), "1", content);
  assert.equal(f.output.snapshot().text, content);
});

test("the transcript is text, including inert HTML and terminal control sequences", () => {
  const f = fixture(); f.output.select("7");
  const text = '<script>alert(1)</script>\u001b]52;c;secret\u0007';
  delta(f.latest(), "1", text);
  assert.equal(f.output.snapshot().text, text);
});

test("oversized messages are rejected without acknowledging their id", () => {
  const f = fixture({ maxFrameChars: 200 }); f.output.select("7");
  delta(f.latest(), "1", "ok");
  delta(f.latest(), "2", "a".repeat(201));
  assert.equal(f.output.snapshot().text, "ok"); assert.equal(f.output.snapshot().cursor, "1");
  assert.equal(f.timers.size, 1);
});

test("the rolling transcript keeps a suffix across segment boundaries", () => {
  const f = fixture({ maxChars: 8 }); f.output.select("7");
  delta(f.latest(), "1", "12345678"); delta(f.latest(), "2", "9");
  assert.equal(f.output.snapshot().text, "23456789");
  delta(f.latest(), "3", "ABCDEFGHIJK");
  assert.equal(f.output.snapshot().text, "DEFGHIJK");
  assert.equal(f.output.snapshot().discardedChars, 12);
  assert.equal(f.output.snapshot().cursor, "3");
});

test("the segment-count budget also bounds floods of tiny chunks", () => {
  const f = fixture({ maxChunks: 3, maxChars: 100 }); f.output.select("7");
  for (let i = 1; i <= 50; i++) delta(f.latest(), String(i), "x");
  assert.equal(f.output.snapshot().text, "xxx");
  assert.equal(f.output.snapshot().discardedChars, 47);
  assert.equal(f.output.snapshot().cursor, "50");
});

test("empty segments acknowledge progress without consuming the chunk budget", () => {
  const f = fixture({ maxChunks: 1 }); f.output.select("7");
  delta(f.latest(), "1", "kept"); delta(f.latest(), "2", "");
  assert.equal(f.output.snapshot().text, "kept"); assert.equal(f.output.snapshot().cursor, "2");
  assert.equal(f.output.snapshot().discardedChars, 0);
});

test("truncation never produces a lone low surrogate", () => {
  const f = fixture({ maxChars: 3 }); f.output.select("7");
  delta(f.latest(), "1", "x😀ab");
  assert.equal(f.output.snapshot().text, "ab");
  assert.equal(f.output.snapshot().discardedChars, 3);
  f.output.select("7"); delta(f.latest(), "2", "😀a"); delta(f.latest(), "3", "b");
  assert.equal(f.output.snapshot().text, "ab");
});

test("pause closes transport and resume uses the cursor without clearing output", () => {
  const f = fixture(); f.output.select("7"); const old = f.latest();
  delta(old, "12", "before"); f.output.pause(); f.output.pause();
  assert.equal(old.closed, true); assert.equal(f.output.snapshot().status, "paused");
  delta(old, "13", "late"); f.output.resume(); f.output.resume();
  assert.equal(f.sources.length, 2);
  assert.equal(f.latest().url.searchParams.get("since_id"), "12");
  delta(f.latest(), "13", "after"); assert.equal(f.output.snapshot().text, "beforeafter");
});

test("page suspension preserves an explicit pause", () => {
  const f = fixture(); f.output.select("7"); f.output.pause();
  f.output.suspend(); f.output.restore();
  assert.equal(f.sources.length, 1); assert.equal(f.output.snapshot().paused, true);
  f.output.resume(); assert.equal(f.sources.length, 2);
});

test("page suspension cancels replacement timer and rejects its stale callback", () => {
  const f = fixture(); f.output.select("7"); delta(f.latest(), "4", "A"); f.latest().fail();
  const pending = f.timers.values().next().value.fn;
  f.output.suspend(); assert.equal(f.timers.size, 0);
  pending(); assert.equal(f.sources.length, 1);
  f.output.restore(); f.output.restore();
  assert.equal(f.sources.length, 2); assert.equal(f.latest().url.searchParams.get("since_id"), "4");
});

test("close clears captured text, identity, retry timer and received cursor", () => {
  const f = fixture(); f.output.select("7"); delta(f.latest(), "5", "private"); f.latest().fail();
  const callback = f.timers.values().next().value.fn;
  f.output.close(); callback(); f.output.restore();
  assert.equal(f.timers.size, 0); assert.equal(f.sources.length, 1);
  assert.equal(f.output.snapshot().paneId, null); assert.equal(f.output.snapshot().text, "");
  assert.equal(f.output.snapshot().cursor, null); assert.equal(f.output.snapshot().status, "closed");
});

test("retry is bounded and ready alone does not reset a failing loop", () => {
  const f = fixture(); f.output.select("7");
  const delays = [];
  for (let i = 0; i < 7; i++) {
    const old = f.latest(); control(old, "ready"); old.fail(); old.fail();
    assert.equal(f.timers.size, 1); delays.push(f.retry());
  }
  assert.deepEqual(delays, [1000, 2000, 4000, 8000, 16000, 30000, 30000]);
  delta(f.latest(), "1", "delivered"); f.latest().fail(); assert.equal(f.retry(), 1000);
});

test("server error diagnostics are finite and persist until actual delivery", () => {
  const f = fixture(); f.output.select("7"); delta(f.latest(), "1", "A");
  control(f.latest(), "error", { code: "storage_error", message: "private detail" });
  assert.equal(f.output.snapshot().error, "output: storage_error");
  f.retry(); control(f.latest(), "ready");
  assert.equal(f.output.snapshot().error, "output: storage_error");
  delta(f.latest(), "2", "B"); assert.equal(f.output.snapshot().error, null);
  control(f.latest(), "error", { code: "<secret>" });
  assert.equal(f.output.snapshot().error, "output: invalid server error frame");
});

test("transient EventSource reconnects do not create replacement owners", () => {
  const f = fixture(); f.output.select("7"); delta(f.latest(), "1", "A");
  f.latest().fail(false);
  assert.equal(f.sources.length, 1); assert.equal(f.timers.size, 0);
  assert.equal(f.latest().closed, false); assert.equal(f.output.snapshot().status, "connecting");
  control(f.latest(), "ready"); delta(f.latest(), "2", "B");
  assert.equal(f.output.snapshot().text, "AB");
});

test("reconnecting before the first fresh cursor reports the unanchored boundary", () => {
  const f = fixture(); f.output.select("7"); f.latest().fail(); f.retry();
  assert.equal(f.output.snapshot().unanchored, true);
  assert.equal(f.latest().url.searchParams.has("since_id"), false);
  delta(f.latest(), "1", "later"); assert.equal(f.output.snapshot().unanchored, true);
});

test("constructor failures back off and can be stopped", () => {
  const f = fixture({ EventSourceImpl: class { constructor() { throw new Error("private"); } } });
  f.output.select("7"); assert.equal(f.timers.size, 1);
  assert.equal(f.output.snapshot().error, "output connection could not be opened");
  assert.equal(f.retry(), 1000); assert.equal(f.timers.size, 1);
  f.output.close(); assert.equal(f.timers.size, 0);
});

test("flooded updates coalesce for frame-paced rendering", () => {
  const f = fixture(); f.output.takeUpdate(); assert.equal(f.output.takeUpdate(), null);
  f.output.select("7");
  for (let id = 1; id <= 100; id++) delta(f.latest(), String(id), "x");
  assert.equal(f.output.takeUpdate().text.length, 100);
  assert.equal(f.output.takeUpdate(), null);
});

test("invalid bounds are rejected before a source can be opened", () => {
  for (const options of [{ maxChars: 0 }, { maxChunks: -1 }, { maxFrameChars: Infinity },
    { maxChars: 1.5 }, { maxChars: 9 * 1024 * 1024 }, { maxChunks: 32769 }]) {
    assert.throws(() => fixture(options), /invalid transcript limit/);
  }
});

const CONTROL_IDS = [
  "workspace", "pane-output", "pane-output-title", "pane-output-status", "pane-output-warnings",
  "pane-output-text", "pane-output-follow", "pane-output-pause", "pane-output-history",
  "pane-output-close", "pane-output-open", "pane-output-id", "pane-output-form", "pane-output-submit",
  "pane-output-hint", "pane-output-search", "pane-output-find-form", "pane-output-previous",
  "pane-output-find-status", "terminal",
];

function panelFixture(options = {}) {
  const f = fixture(options);
  let focused = null;
  let selected = "7";
  class Element {
    constructor(id) {
      this.id = id; this.dataset = {}; this.hidden = false; this.disabled = true;
      this.value = ""; this.checked = id === "pane-output-follow";
      this.selectionStart = this.selectionEnd = this.scrollTop = 0;
      this.scrollHeight = 1000; this.clientHeight = 100;
      this.handlers = new Map(); this.attributes = {}; this.writes = 0; this.text = "";
    }
    set textContent(value) { this.text = value; this.writes++; }
    get textContent() { return this.text; }
    set innerHTML(_) { throw new Error("capture content must never enter innerHTML"); }
    addEventListener(type, handler) { this.handlers.set(type, handler); }
    setAttribute(name, value) { this.attributes[name] = value; }
    focus() { focused = this.id; }
    setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; }
    emit(type, values = {}) {
      const event = { ...values, prevented: false, preventDefault() { this.prevented = true; } };
      this.handlers.get(type)?.(event); return event;
    }
  }
  const nodes = Object.fromEntries(CONTROL_IDS.map((id) => [id, new Element(id)]));
  const document = { getElementById: (id) => nodes[id] };
  const panel = mountPaneOutput({ document, output: f.output, getSelectedPane: () => selected });
  return { ...f, panel, nodes, focused: () => focused, selectRow: (id) => { selected = id; } };
}

test("panel opens the selected pane, renders literal text, and is frame paced", () => {
  const f = panelFixture();
  assert.equal(f.nodes["pane-output"].hidden, true);
  assert.equal(f.nodes["pane-output-open"].disabled, false);
  f.nodes["pane-output-open"].emit("click");
  assert.equal(f.nodes.workspace.dataset.outputOpen, "true");
  assert.equal(f.nodes["pane-output-title"].textContent, "Pane 7 — captured output");
  assert.equal(f.focused(), "pane-output-text");
  delta(f.latest(), "1", "<b>not HTML</b>");
  assert.equal(f.nodes["pane-output-text"].value, "");
  f.panel.render();
  assert.equal(f.nodes["pane-output-text"].value, "<b>not HTML</b>");
});

test("changing fleet selection cannot retarget an already opened transcript", () => {
  const f = panelFixture(); f.panel.openSelected(); const first = f.latest();
  f.selectRow("8"); f.panel.render();
  assert.equal(f.sources.length, 1);
  delta(first, "1", "seven"); f.panel.render();
  assert.equal(f.nodes["pane-output-title"].textContent, "Pane 7 — captured output");
  f.panel.openSelected();
  assert.equal(first.closed, true);
  assert.equal(f.nodes["pane-output-text"].value, "");
  assert.equal(f.latest().url.searchParams.get("pane_id"), "8");
});

test("manual pane form preserves exact IDs and rejects invalid input without retargeting", () => {
  const f = panelFixture();
  f.nodes["pane-output-id"].value = "18446744073709551615";
  assert.equal(f.nodes["pane-output-form"].emit("submit").prevented, true);
  assert.equal(f.latest().url.searchParams.get("pane_id"), "18446744073709551615");
  const first = f.latest();
  f.nodes["pane-output-id"].value = "bad"; f.nodes["pane-output-form"].emit("submit");
  assert.equal(f.latest(), first); assert.equal(first.closed, false);
  assert.match(f.nodes["pane-output-hint"].textContent, /valid unsigned pane ID/);
});

test("an empty selected row does not accidentally open an all-pane stream", () => {
  const f = panelFixture(); f.selectRow(undefined); f.panel.openSelected();
  assert.equal(f.sources.length, 0); assert.match(f.nodes["pane-output-hint"].textContent, /Select a pane/);
});

test("history button restarts only the pinned pane with explicit replay", () => {
  const f = panelFixture(); f.panel.openSelected(); delta(f.latest(), "100", "fresh"); f.panel.render();
  f.selectRow("8"); f.nodes["pane-output-history"].emit("click");
  assert.equal(f.latest().url.searchParams.get("pane_id"), "7");
  assert.equal(f.latest().url.searchParams.get("since_id"), "0");
  assert.equal(f.nodes["pane-output-text"].value, "");
  assert.match(f.nodes["pane-output-status"].textContent, /retained history/);
});

test("panel search freezes output, selects literal matches, wraps and resumes from its cursor", () => {
  const f = panelFixture(); f.panel.openSelected();
  delta(f.latest(), "9", "foo bar foo"); f.panel.render();
  const original = f.latest();
  f.nodes["pane-output-search"].value = "foo";
  f.nodes["pane-output-find-form"].emit("submit");
  assert.equal(original.closed, true); assert.equal(f.output.snapshot().paused, true);
  const area = f.nodes["pane-output-text"];
  assert.equal(area.selectionStart, 0); assert.equal(area.selectionEnd, 3);
  f.nodes["pane-output-find-form"].emit("submit"); assert.equal(area.selectionStart, 8);
  f.nodes["pane-output-find-form"].emit("submit"); assert.equal(area.selectionStart, 0);
  f.nodes["pane-output-previous"].emit("click"); assert.equal(area.selectionStart, 8);
  f.nodes["pane-output-pause"].emit("click");
  assert.equal(f.latest().url.searchParams.get("since_id"), "9");
  assert.equal(f.output.snapshot().paused, false);
});

test("search treats regex syntax as text and reports missing text without modifying output", () => {
  const f = panelFixture(); f.panel.openSelected(); delta(f.latest(), "1", "prefix [a-z]+ suffix"); f.panel.render();
  f.nodes["pane-output-search"].value = "[a-z]+"; f.nodes["pane-output-find-form"].emit("submit");
  assert.equal(f.nodes["pane-output-text"].selectionStart, 7);
  f.nodes["pane-output-search"].value = "absent"; f.nodes["pane-output-find-form"].emit("submit");
  assert.match(f.nodes["pane-output-find-status"].textContent, /Not found/);
  assert.equal(f.nodes["pane-output-text"].value, "prefix [a-z]+ suffix");
});

test("manual scrolling disables follow and updates preserve the viewport", () => {
  const f = panelFixture(); f.panel.openSelected(); delta(f.latest(), "1", "initial"); f.panel.render();
  const area = f.nodes["pane-output-text"];
  area.scrollTop = 25; area.emit("scroll");
  assert.equal(f.nodes["pane-output-follow"].checked, false);
  area.setSelectionRange(1, 3);
  delta(f.latest(), "2", "more"); f.panel.render();
  assert.equal(area.scrollTop, 25); assert.equal(area.selectionStart, 1); assert.equal(area.selectionEnd, 3);
  f.nodes["pane-output-follow"].checked = true; f.nodes["pane-output-follow"].emit("change");
  assert.equal(area.scrollTop, area.scrollHeight);
});

test("close and Escape clear sensitive retained output and restore canvas focus", () => {
  const f = panelFixture(); f.panel.openSelected(); delta(f.latest(), "1", "private"); f.panel.render();
  assert.equal(f.nodes["pane-output"].emit("keydown", { key: "Escape" }).prevented, true);
  assert.equal(f.nodes["pane-output-text"].value, "");
  assert.equal(f.nodes["pane-output"].hidden, true);
  assert.equal(f.nodes.workspace.dataset.outputOpen, "false");
  assert.equal(f.focused(), "terminal"); assert.equal(f.latest().closed, true);
});

test("suspension preserves transcript and explicit pause through panel controls", () => {
  const f = panelFixture(); f.panel.openSelected(); delta(f.latest(), "1", "kept"); f.panel.render();
  f.nodes["pane-output-pause"].emit("click");
  f.panel.suspend(); f.panel.restore();
  assert.equal(f.sources.length, 1); assert.equal(f.nodes["pane-output-pause"].textContent, "Resume");
  assert.equal(f.nodes["pane-output-text"].value, "kept");
});

test("output floods do not repeatedly rewrite unchanged live-region status", () => {
  const f = panelFixture({ maxChars: 5 }); f.panel.openSelected();
  delta(f.latest(), "1", "hello"); f.panel.render();
  const statusWrites = f.nodes["pane-output-status"].writes;
  for (let id = 2; id <= 10; id++) { delta(f.latest(), String(id), "x"); f.panel.render(); }
  assert.equal(f.nodes["pane-output-status"].writes, statusWrites);
  assert.match(f.nodes["pane-output-warnings"].textContent, /Older text/);
  control(f.latest(), "gap"); f.panel.render();
  assert.match(f.nodes["pane-output-warnings"].textContent, /continuity is not guaranteed/);
});

function keyboardFixture() {
  const canvas = {};
  const calls = [];
  const dependencies = {
    canvas,
    term: {
      input: (event) => calls.push(["input", event.key]),
      drainEncodedInputs: () => ["encoded"],
      drainEncodedInputBytes: () => calls.push(["drain-bytes"]),
      drainImeCompositionJsonl: () => calls.push(["drain-ime"]),
    },
    viewer: { pushEncodedInput: (json) => calls.push(["viewer", json]) },
    inspector: { openSelected: () => calls.push(["inspect"]) },
  };
  const key = (values) => {
    const event = { target: canvas, key: "j", prevented: false, preventDefault() { this.prevented = true; }, ...values };
    handleFleetKey(event, dependencies); return event;
  };
  return { calls, key };
}

test("native controls, Tab, and browser shortcuts never reach fleet input", () => {
  const f = keyboardFixture();
  for (const values of [{ target: {} }, { key: "Tab" }, { metaKey: true }, { ctrlKey: true }, { altKey: true }]) {
    assert.equal(f.key(values).prevented, false);
  }
  assert.deepEqual(f.calls, []);
});

test("Enter inspects once without sending input bytes; navigation stays in the viewer", () => {
  const f = keyboardFixture();
  assert.equal(f.key({ key: "Enter" }).prevented, true);
  f.key({ key: "Enter", repeat: true });
  assert.deepEqual(f.calls, [["inspect"]]);
  assert.equal(f.key({ key: "ArrowDown" }).prevented, true);
  assert.deepEqual(f.calls.slice(1), [["input", "ArrowDown"], ["viewer", "encoded"], ["drain-bytes"], ["drain-ime"]]);
});

test("packaged page contains the required accessible controls and module", () => {
  const html = readFileSync(new URL("../../crates/frankenterm-browser-ui/web/index.html", import.meta.url), "utf8");
  const ids = [...html.matchAll(/\bid="([^"]+)"/g)].map((match) => match[1]);
  assert.equal(ids.length, new Set(ids).size, "DOM IDs must be unique");
  for (const id of CONTROL_IDS) assert.ok(ids.includes(id), `missing ${id}`);
  assert.match(html, /<textarea[^>]*id="pane-output-text"[^>]*readonly/);
  const builder = readFileSync(new URL("../../scripts/build-browser-ui.sh", import.meta.url), "utf8");
  assert.ok(builder.includes('"$crate_dir/web/pane-output.mjs"'), "package must ship the imported module");
  const wasm = readFileSync(new URL("../../crates/frankenterm-browser-ui/src/wasm.rs", import.meta.url), "utf8");
  assert.ok(wasm.includes("js_name = selectedPaneId"));
  assert.ok(wasm.includes("pub fn selected_pane_id(&self) -> Option<String>"));
});
