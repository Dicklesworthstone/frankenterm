import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { mountFleetApp } from "../../crates/frankenterm-browser-ui/web/app.mjs";

const flush = async () => { for (let n = 0; n < 60; n++) await Promise.resolve(); };
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
class Events {
  constructor() { this.listeners = new Map(); }
  addEventListener(type, fn) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(fn);
  }
  emit(type, values = {}) {
    const event = { type, target: this, prevented: false, preventDefault() { this.prevented = true; }, ...values };
    for (const fn of this.listeners.get(type) ?? []) fn(event);
    return event;
  }
}
class Node extends Events {
  constructor(doc, tag = "div") {
    super(); this.doc = doc; this.tag = tag; this.children = []; this.attrs = {}; this.dataset = {};
    this.value = ""; this.text = ""; this.textWrites = 0; this.disabled = false; this.hidden = false;
    this.clientWidth = 800; this.clientHeight = 480; this.scrollHeight = 480; this.scrollTop = 0;
    this.selectionStart = this.selectionEnd = 0; this.checked = false; this.open = false;
  }
  set textContent(value) { this.text = value; this.textWrites += 1; }
  get textContent() { return this.text; }
  set innerHTML(_) { throw new Error("captured data must stay native text"); }
  setAttribute(key, value) { this.attrs[key] = value; }
  append(...children) { for (const child of children) child.parent = this; this.children.push(...children); }
  replaceChildren(...children) { this.children = []; this.append(...children); }
  contains(node) { return node === this || this.children.some((child) => child.contains(node)); }
  focus() { if (!this.hidden && !this.disabled) this.doc.activeElement = this; }
  showModal() { this.open = true; }
  close() { this.open = false; this.emit("close"); }
  requestSubmit() { this.emit("submit"); }
  setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; }
}

function fixture(t, options = {}) {
  const doc = new Events();
  doc.documentElement = { dataset: {} };
  doc.body = new Node(doc, "body"); doc.activeElement = doc.body;
  doc.fonts = { load: async () => [] };
  doc.createElement = (tag) => new Node(doc, tag);
  const nodes = new Map();
  const html = readFileSync(new URL("../../crates/frankenterm-browser-ui/web/index.html", import.meta.url), "utf8");
  for (const [, tag, attrs, id, rest] of html.matchAll(/<([a-z]+)([^>]*?)\bid="([^"]+)"([^>]*)>/g)) {
    const node = new Node(doc, tag);
    node.disabled = /\bdisabled\b/.test(attrs + rest);
    node.hidden = /\bhidden\b/.test(attrs + rest);
    nodes.set(id, node);
  }
  doc.getElementById = (id) => nodes.get(id);
  const get = (id) => nodes.get(id);
  get("history-mode").value = "phrase"; get("history-limit").value = "50";
  get("pane-output-follow").checked = true;
  if (options.hugeViewport) get("fleet-viewport").clientWidth = get("fleet-viewport").clientHeight = 100_000;
  const host = new Events();
  host.location = { href: "https://example.test/prefix/ui/" }; host.devicePixelRatio = 1;
  const selection = { isCollapsed: true, rangeCount: 0, anchorNode: null, focusNode: null,
    removeAllRanges() { this.isCollapsed = true; this.rangeCount = 0; this.anchorNode = this.focusNode = null; } };
  host.getSelection = () => selection;
  const frames = new Map(), timers = new Map(), intervals = new Map();
  let handle = 0;
  const sources = [], requests = [], viewers = [], renderers = [], observers = [];
  class Source extends Events {
    static CLOSED = 2;
    constructor(url) { super(); this.url = new URL(url); this.readyState = 1; this.closed = false; sources.push(this); }
    close() { this.closed = true; this.readyState = Source.CLOSED; }
    message(type, frame, lastEventId = "") { this.emit(type, { data: JSON.stringify(frame), lastEventId }); }
  }
  class Viewer {
    constructor(cols, rows) {
      this.size = { cols, rows }; this.resizes = []; this.inputs = []; this.pending = [];
      this.selected = "7"; this.view = "panes"; this.revision = 0; this.dirty = true;
      this.drains = 0; this.bodies = []; this.refreshRequested = false; viewers.push(this);
    }
    init() { if (options.modelInitFailure) throw new Error("private WASM failure"); }
    setTime() { this.dirty = true; }
    resize(cols, rows) { this.size = { cols, rows }; this.resizes.push(this.size); this.dirty = true; return true; }
    step() {
      if (this.failStep) throw new Error("private model trap");
      for (const input of this.pending.splice(0)) {
        if (input.key === "ArrowDown") this.selected = "18446744073709551615";
        if (input.key === "ArrowUp" || input.key === "Home") this.selected = "7";
        if (input.key === "End") this.selected = "42";
        if (input.key === "1") this.view = "panes";
        if (input.key === "2") this.view = "detections";
        if (input.key === "r") this.refreshRequested = true;
      }
      const dirty = this.dirty; this.dirty = false;
      if (dirty) this.revision += 1;
      return dirty;
    }
    screenText() { return this.overrideText ?? `${this.view} frame ${this.revision}\nPane ${this.selected} <not-markup>`; }
    selectedPaneId() { return this.selected; }
    pushEncodedInput(json) { const input = JSON.parse(json); this.inputs.push(input); this.pending.push(input); this.dirty = true; return true; }
    takeFlatPatches() { this.drains += 1; return { cells: new Uint32Array([0, 1, 65, 0]), spans: new Uint32Array([0, 1]) }; }
    takeRefreshRequest() { const requested = this.refreshRequested; this.refreshRequested = false; return requested; }
    ingestPanes(body) { this.bodies.push(["panes", body]); this.dirty = true; }
    ingestEvents(body) { this.bodies.push(["events", body]); this.dirty = true; }
    ingestStreamFrame() { this.dirty = true; }
    setStreamState(state) { this.streamState = state; this.dirty = true; }
    reportError(error) { this.error = error; this.dirty = true; }
  }
  const rendererGate = deferred(), modelGate = deferred(), initGate = deferred();
  class Renderer {
    constructor() { this.fits = []; this.patches = 0; this.renders = 0; this.destroyed = 0; renderers.push(this); }
    apiContract() { return options.badContract ? { apiLine: "other", apiVersion: "9" }
      : { apiLine: "frankenterm-js", apiVersion: "1.0.0" }; }
    async init() {
      this.initialized = true;
      if (options.rendererFailure === "init") throw new Error("private GPU diagnostics");
      if (options.pendingRendererInit) await initGate.promise;
    }
    fitToContainer(width, height) {
      this.fits.push({ width, height });
      return options.badGeometry ? { cols: 1001, rows: 401 }
        : { cols: Math.max(20, Math.floor(width / 8)), rows: Math.max(4, Math.floor(height / 16)) };
    }
    applyPatchBatchFlat() { this.patches += 1; }
    render() { if (this.failRender) throw new Error("private device loss"); this.renders += 1; }
    destroy() { this.destroyed += 1; }
    input(input) { this.inputQueue = [JSON.stringify(input)]; }
    drainEncodedInputs() { return this.inputQueue?.splice(0) ?? []; }
    drainEncodedInputBytes() { return new Uint8Array(); }
    drainImeCompositionJsonl() { return ""; }
  }
  const viewerModule = { default: async () => {}, FleetViewer: Viewer };
  const rendererModule = { default: async () => {}, FrankenTermWeb: Renderer };
  const fetchImpl = async (url, init) => {
    const request = { url: new URL(url), ...init }; requests.push(request);
    if (request.url.pathname.endsWith("/search")) return new Response(JSON.stringify({ ok: true, data: {
      results: [{ segment_id: 3, pane_id: 7, score: -0.5, captured_at: 1000, content_len: 6, snippet: "needle" }], total: 1,
    } }));
    if (options.pendingSnapshots) return new Promise(() => {});
    return new Response('{"ok":true,"data":[]}');
  };
  const app = mountFleetApp({
    document: doc, window: host, fetchImpl, EventSourceImpl: Source,
    loadViewer: () => options.pendingModel ? modelGate.promise : options.modelImportFailure
      ? Promise.reject(new Error("private missing model")) : Promise.resolve(viewerModule),
    loadRenderer: () => options.pendingRenderer ? rendererGate.promise : options.rendererFailure === "import"
      ? Promise.reject(new Error("private missing renderer")) : Promise.resolve(rendererModule),
    requestFrame: (fn) => { const id = ++handle; frames.set(id, fn); return id; },
    cancelFrame: (id) => frames.delete(id),
    setTimer: (fn, delay) => { const id = ++handle; timers.set(id, { fn, delay }); return id; },
    clearTimer: (id) => timers.delete(id),
    setRepeating: (fn, delay) => { const id = ++handle; intervals.set(id, { fn, delay }); return id; },
    clearRepeating: (id) => intervals.delete(id),
    ResizeObserverImpl: class { constructor(fn) { this.callback = fn; observers.push(this); } observe(node) { this.node = node; } },
    now: () => 10_000,
  });
  t.after(() => app.suspend());
  const frame = () => {
    assert.equal(frames.size, 1);
    const [id, fn] = frames.entries().next().value; frames.delete(id); fn();
  };
  const openPane = (id = "7") => { get("pane-output-id").value = id; get("pane-output-form").requestSubmit(); };
  return { app, doc, host, get, sources, requests, viewers, renderers, frames, timers, intervals, observers,
    selection, frame, openPane,
    resolveModel: () => modelGate.resolve(viewerModule), rejectModel: () => modelGate.reject(new Error("missing model")),
    resolveRenderer: () => rendererGate.resolve(rendererModule), rejectRenderer: () => rendererGate.reject(new Error("missing renderer")),
    resolveRendererInit: () => initGate.resolve(),
    key: (key, target = get("screen-text"), extra = {}) => host.emit("keydown", { target, key, code: key, ...extra }),
    fleetSources: () => sources.filter((source) => source.url.pathname.endsWith("/stream/events")),
  };
}

test("manual output and real history search work before either WASM import settles", async (t) => {
  const f = fixture(t, { pendingModel: true, pendingRenderer: true });
  assert.equal(f.get("pane-output-submit").disabled, false);
  assert.equal(f.get("history-search-open").disabled, false);
  assert.equal(f.get("pane-output-open").disabled, true);
  assert.equal(f.fleetSources().length, 0);
  f.openPane();
  const source = f.sources.at(-1);
  source.message("delta", { schema: "ft.stream.v1", stream: "deltas", kind: "delta", data: {
    pane_id: 7, segment_id: 1, seq: 0, content: "<script>captured text</script>",
  } }, "delta-v1:pane:7:1");
  f.frame();
  assert.equal(f.get("pane-output-text").value, "<script>captured text</script>");
  f.get("history-search-open").emit("click");
  f.get("history-query").value = "needle";
  f.get("history-search-form").requestSubmit();
  await flush();
  assert.equal(f.requests[0].url.pathname, "/prefix/search");
  assert.equal(f.get("history-results").children.length, 1);
  assert.match(f.get("history-status").textContent, /1 ranked match/);
  f.rejectModel(); f.rejectRenderer(); await f.app.ready;
  assert.equal(f.doc.documentElement.dataset.viewerMode, "capture-only");
  assert.equal(source.closed, false);
  assert.equal(f.get("pane-output-submit").disabled, false);
  assert.doesNotMatch(f.get("fleet-mode").textContent, /private|missing model/);
  f.get("pane-output-close").emit("click");
  assert.equal(f.doc.activeElement, f.get("pane-output-id"));
});

test("renderer import failure leaves one live fleet model with native exact-pane navigation", async (t) => {
  const f = fixture(t, { rendererFailure: "import" }); await f.app.ready; await flush(); f.frame();
  assert.equal(f.viewers.length, 1); assert.equal(f.fleetSources().length, 1);
  assert.equal(f.get("terminal").hidden, true);
  assert.equal(f.get("screen-text").className, "fleet-text");
  assert.match(f.get("screen-text").textContent, /panes frame/);
  f.get("fleet-next").emit("click");
  assert.deepEqual(f.viewers[0].inputs.at(-1), { kind: "key", phase: "down", key: "ArrowDown", code: "", mods: 0, repeat: false });
  assert.match(f.get("fleet-selection").textContent, /18446744073709551615/);
  assert.equal(f.key("Enter").prevented, true);
  const source = f.sources.at(-1);
  assert.equal(source.url.pathname, "/prefix/stream/deltas");
  assert.equal(source.url.searchParams.get("pane_id"), "18446744073709551615");
  f.get("pane-output-close").emit("click");
  assert.equal(f.doc.activeElement, f.get("screen-text"));
  f.get("fleet-events").emit("click");
  assert.match(f.get("screen-text").textContent, /detections frame/);
  assert.equal(f.fleetSources().length, 1);
});

test("GPU initialization and API-contract failures release the candidate and keep text usable", async (t) => {
  for (const options of [{ rendererFailure: "init" }, { badContract: true }]) {
    const f = fixture(t, options); await f.app.ready;
    assert.equal(f.renderers[0].destroyed, 1);
    assert.equal(f.doc.documentElement.dataset.viewerMode, "text");
    assert.match(f.get("fleet-mode").textContent, /Canvas rendering is unavailable/);
    assert.doesNotMatch(f.get("fleet-mode").textContent, /private GPU/);
    f.get("fleet-last").emit("click");
    assert.match(f.get("fleet-selection").textContent, /Selected pane 42/);
    assert.equal(f.get("history-search-open").disabled, false);
  }
});

test("model import or initialization failure leaves manual inspection available without a fleet source", async (t) => {
  for (const options of [{ modelImportFailure: true }, { modelInitFailure: true }]) {
    const f = fixture(t, options); await f.app.ready;
    assert.equal(f.doc.documentElement.dataset.viewerMode, "capture-only");
    assert.equal(f.fleetSources().length, 0);
    assert.equal(f.renderers.length, 0);
    assert.equal(f.get("fleet-next").disabled, true);
    assert.equal(f.get("pane-output-open").disabled, true);
    f.openPane("42");
    assert.equal(f.sources.at(-1).url.searchParams.get("pane_id"), "42");
    f.get("history-search-open").emit("click");
    assert.equal(f.get("history-search").open, true);
  }
});

test("a late canvas attaches with a full resize repaint and preserves the one model and stream", async (t) => {
  const f = fixture(t, { pendingRenderer: true }); await flush();
  assert.equal(f.doc.documentElement.dataset.viewerMode, "text");
  f.get("fleet-next").emit("click");
  f.get("pane-output-id").focus();
  const model = f.viewers[0], source = f.fleetSources()[0], resizes = model.resizes.length;
  f.resolveRenderer(); await f.app.ready;
  assert.equal(f.doc.documentElement.dataset.viewerMode, "canvas");
  assert.equal(f.viewers.length, 1); assert.equal(f.viewers[0], model);
  assert.equal(f.fleetSources().length, 1); assert.equal(f.fleetSources()[0], source);
  assert.equal(model.selected, "18446744073709551615");
  assert.ok(model.resizes.length > resizes);
  assert.ok(f.renderers[0].patches > 0); assert.ok(f.renderers[0].renders > 0);
  assert.equal(f.doc.activeElement, f.get("pane-output-id"));
});

test("late rendering keeps an active text reader in place until an explicit canvas toggle", async (t) => {
  const f = fixture(t, { pendingRenderer: true }); await flush();
  f.get("screen-text").focus();
  f.resolveRenderer(); await f.app.ready;
  assert.equal(f.doc.documentElement.dataset.viewerMode, "text");
  assert.equal(f.doc.activeElement, f.get("screen-text"));
  assert.equal(f.get("fleet-text-toggle").disabled, false);
  f.get("fleet-text-toggle").focus();
  f.get("fleet-text-toggle").emit("click");
  assert.equal(f.doc.documentElement.dataset.viewerMode, "canvas");
  assert.equal(f.doc.activeElement, f.get("fleet-text-toggle"));
  f.get("fleet-text-toggle").emit("click");
  assert.equal(f.doc.documentElement.dataset.viewerMode, "text");
  assert.equal(f.fleetSources().length, 1);
});

test("device loss during painting preserves live text, focus and existing network ownership", async (t) => {
  const f = fixture(t); await f.app.ready; await flush();
  const source = f.fleetSources()[0];
  f.get("terminal").focus();
  f.renderers[0].failRender = true; f.viewers[0].dirty = true; f.frame();
  assert.equal(f.doc.documentElement.dataset.viewerMode, "text");
  assert.equal(f.renderers[0].destroyed, 1);
  assert.equal(f.doc.activeElement, f.get("screen-text"));
  assert.equal(source.closed, false); assert.equal(f.fleetSources().length, 1);
  assert.match(f.get("screen-text").textContent, /Pane 7/);
  f.get("fleet-next").emit("click");
  assert.match(f.get("screen-text").textContent, /18446744073709551615/);
});

test("pagehide during imports prevents late startup from opening streams; pageshow has one owner", async (t) => {
  const f = fixture(t, { pendingModel: true, pendingRenderer: true, pendingSnapshots: true });
  f.host.emit("pagehide");
  assert.equal(f.frames.size, 0);
  f.resolveModel(); f.resolveRenderer(); await f.app.ready;
  assert.equal(f.sources.length, 0); assert.equal(f.requests.length, 0);
  assert.equal(f.frames.size, 0); assert.equal(f.intervals.size, 0);
  f.host.emit("pageshow"); f.host.emit("pageshow"); await flush();
  assert.equal(f.fleetSources().length, 1); assert.equal(f.frames.size, 1); assert.equal(f.intervals.size, 1);
  assert.equal(f.requests.length, 2);
  f.host.emit("pagehide"); await flush();
  assert.ok(f.requests.every((request) => request.signal.aborted));
  assert.equal(f.fleetSources()[0].closed, true);
  assert.equal(f.frames.size, 0); assert.equal(f.intervals.size, 0); assert.equal(f.timers.size, 0);
  f.host.emit("pageshow"); f.host.emit("pageshow"); await flush();
  assert.equal(f.fleetSources().length, 2);
  assert.equal(f.fleetSources().filter((source) => !source.closed).length, 1);
  assert.equal(f.frames.size, 1); assert.equal(f.intervals.size, 1);
});

test("pagehide while GPU initialization awaits cannot restart a suspended fleet", async (t) => {
  const f = fixture(t, { pendingRendererInit: true }); await flush();
  assert.equal(f.fleetSources().length, 1);
  f.host.emit("pagehide");
  f.resolveRendererInit(); await f.app.ready;
  assert.equal(f.fleetSources().length, 1); assert.equal(f.fleetSources()[0].closed, true);
  assert.equal(f.frames.size, 0); assert.equal(f.intervals.size, 0);
  f.host.emit("pageshow");
  assert.equal(f.fleetSources().length, 2);
  assert.ok(f.renderers[0].patches > 0);
});

test("native copying holds text steady while draining model frames, then resumes the latest frame", async (t) => {
  const f = fixture(t, { rendererFailure: "import" }); await f.app.ready; await flush(); f.frame();
  const text = f.get("screen-text"), model = f.viewers[0];
  Object.assign(f.selection, { isCollapsed: false, rangeCount: 1, anchorNode: text, focusNode: text });
  f.doc.emit("selectionchange");
  const before = text.textContent, writes = text.textWrites, drains = model.drains;
  model.dirty = true; f.frame();
  assert.equal(text.textContent, before); assert.equal(text.textWrites, writes);
  assert.ok(model.drains > drains);
  assert.match(f.get("fleet-mode").textContent, /held steady/);
  f.selection.removeAllRanges(); f.doc.emit("selectionchange");
  assert.notEqual(text.textContent, before);
  assert.doesNotMatch(f.get("fleet-mode").textContent, /held steady/);
});

test("text mode preserves native shortcuts, composition, Shift selection, Tab and control focus", async (t) => {
  const f = fixture(t, { rendererFailure: "import" }); await f.app.ready;
  for (const [key, target, extra] of [
    ["Tab"], ["c", undefined, { ctrlKey: true }], ["a", undefined, { metaKey: true }],
    ["ArrowDown", undefined, { shiftKey: true }], ["ArrowDown", undefined, { isComposing: true }],
    ["2", f.get("pane-output-id")], ["ArrowUp", f.get("pane-output-text")], ["PageDown"],
  ]) assert.equal(f.key(key, target, extra).prevented, false);
  assert.equal(f.viewers[0].inputs.length, 0);
  assert.equal(f.key("ArrowDown").prevented, true);
  assert.deepEqual(f.viewers[0].inputs.at(-1), { kind: "key", phase: "down", key: "ArrowDown", code: "ArrowDown", mods: 0, repeat: false });
  assert.equal(f.key("Enter", undefined, { repeat: true }).prevented, true);
  assert.equal(f.sources.length, 1, "a repeated Enter must not open output");
});

test("fleet geometry stays bounded and an invalid canvas geometry falls back to readable text", async (t) => {
  for (const options of [{ hugeViewport: true }, { hugeViewport: true, badGeometry: true },
    { hugeViewport: true, rendererFailure: "import" }]) {
    const f = fixture(t, options); await f.app.ready;
    assert.ok(f.viewers[0].resizes.every(({ cols, rows }) => cols <= 240 && rows <= 100));
    assert.ok(f.renderers.every((renderer) => renderer.fits.every(({ width, height }) => width <= 1920 && height <= 1600)));
    if (options.badGeometry) assert.equal(f.doc.documentElement.dataset.viewerMode, "text");
    assert.match(f.get("screen-text").textContent, /Pane 7/);
  }
});

test("a model trap or oversized text frame retires fleet traffic but leaves captured-output tools running", async (t) => {
  for (const fault of ["trap", "oversized"]) {
    const f = fixture(t, { rendererFailure: "import" }); await f.app.ready; await flush();
    if (fault === "trap") f.viewers[0].failStep = true;
    else f.viewers[0].overrideText = "x".repeat(48_101);
    f.viewers[0].dirty = true; f.frame();
    assert.equal(f.doc.documentElement.dataset.viewerMode, "capture-only");
    assert.equal(f.fleetSources()[0].closed, true); assert.equal(f.intervals.size, 0);
    assert.equal(f.frames.size, 1, "manual output still owns its rendering loop");
    assert.ok(f.get("screen-text").textContent.length < 200);
    f.openPane();
    assert.equal(f.sources.at(-1).url.pathname, "/prefix/stream/deltas");
    assert.equal(f.get("history-search-open").disabled, false);
  }
});

test("session disposal clears captured data and cannot resume on pageshow", async (t) => {
  const f = fixture(t); await f.app.ready; await flush(); f.frame();
  f.openPane();
  const outputSource = f.sources.at(-1);
  outputSource.message("delta", { schema: "ft.stream.v1", stream: "deltas", kind: "delta", data: {
    pane_id: 7, segment_id: 1, seq: 0, content: "private captured session output",
  } }, "delta-v1:pane:7:1");
  f.frame();
  f.get("history-search-open").emit("click");
  f.get("history-query").value = "private query";
  f.get("history-search-form").requestSubmit(); await flush();
  assert.match(f.get("pane-output-text").value, /private captured/);
  assert.equal(f.get("history-results").children.length, 1);
  const sourceCount = f.sources.length;
  f.app.dispose();
  assert.equal(f.get("pane-output-text").value, "");
  assert.equal(f.get("pane-output-id").value, "");
  assert.equal(f.get("screen-text").textContent, "");
  assert.equal(f.get("history-query").value, "");
  assert.equal(f.get("history-results").children.length, 0);
  assert.ok(f.sources.every((source) => source.closed));
  assert.equal(f.frames.size, 0);
  assert.equal(f.intervals.size, 0);
  assert.equal(f.renderers[0].destroyed, 1);
  f.host.emit("pageshow"); f.app.resume(); await flush();
  assert.equal(f.sources.length, sourceCount);
  assert.equal(f.frames.size, 0);
  assert.equal(f.get("screen-text").textContent, "");
});

test("session disposal fences late WASM imports and pending GPU initialization", async (t) => {
  const pending = fixture(t, { pendingModel: true, pendingRenderer: true });
  await flush(); pending.app.dispose();
  pending.resolveModel(); pending.resolveRenderer(); await pending.app.ready;
  assert.equal(pending.viewers.length, 0);
  assert.equal(pending.renderers.length, 0);
  assert.equal(pending.sources.length, 0);
  assert.equal(pending.get("screen-text").textContent, "");
  const gpu = fixture(t, { pendingRendererInit: true }); await flush();
  assert.equal(gpu.renderers.length, 1);
  gpu.app.dispose(); gpu.resolveRendererInit(); await gpu.app.ready;
  assert.equal(gpu.renderers[0].destroyed, 1);
  assert.ok(gpu.sources.every((source) => source.closed));
  assert.equal(gpu.get("screen-text").textContent, "");
  assert.equal(gpu.doc.documentElement.dataset.viewerReady, "locked");
});
