import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { mountFleetApp } from "../../crates/frankenterm-browser-ui/web/app.mjs";

const OUTPUT_EPOCH = "0123456789abcdef0123456789abcdef";
const deltaCursor = (id, pane = "7") => `delta-v2:${OUTPUT_EPOCH}:pane:${pane}:${id}`;

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
  remove() {
    if (this.parent) this.parent.children = this.parent.children.filter((child) => child !== this);
    this.parent = null;
  }
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
  doc.visibilityState = options.hidden ? "hidden" : "visible";
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
  const sources = [], requests = [], viewers = [], renderers = [], observers = [], notifications = [];
  class Notification {
    static permission = "granted";
    static async requestPermission() { return Notification.permission; }
    constructor(title, config) { this.title = title; this.config = config; this.closed = false; notifications.push(this); }
    close() { this.closed = true; this.onclose?.(); }
  }
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
  const rendererGate = deferred(), modelGate = deferred(), initGate = deferred(), searchGate = deferred();
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
    if (request.url.pathname.endsWith("/search") && options.pendingSearch) return searchGate.promise;
    if (request.url.pathname.endsWith("/search")) return new Response(JSON.stringify({ ok: true, data: {
      results: [{ segment_id: 3, pane_id: 7, score: -0.5, captured_at: 1000, content_len: 6, snippet: "needle", capture_cursor: deltaCursor("2") }], total: 1,
    } }));
    if (options.pendingSnapshots) return new Promise(() => {});
    if (request.url.pathname.endsWith("/panes") && options.paneSnapshot) return new Response(options.paneSnapshot);
    return new Response('{"ok":true,"data":[]}');
  };
  const app = mountFleetApp({
    document: doc, window: host, fetchImpl, EventSourceImpl: Source,
    NotificationImpl: options.notifications ? Notification : undefined,
    notificationStorage: null,
    verifyNotificationSession: options.verifyNotificationSession ?? (async () => true),
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
    now: options.now ?? (() => 10_000),
  });
  t.after(() => app.dispose());
  const frame = () => {
    assert.equal(frames.size, 1);
    const [id, fn] = frames.entries().next().value; frames.delete(id); fn();
  };
  const openPane = (id = "7") => {
    get("pane-output-id").value = id;
    get("pane-output-form").requestSubmit();
    const source = sources.at(-1);
    const cursor = deltaCursor("0", id);
    source.emit("ready", {
      data: `{"schema":"ft.stream.v1","stream":"deltas","kind":"ready","data":{"pane_id":${id},"resume_after_id":0,"cursor":${JSON.stringify(cursor)}}}`,
      lastEventId: cursor,
    });
  };
  return { app, doc, host, get, sources, requests, viewers, renderers, frames, timers, intervals, observers,
    selection, frame, openPane, notifications, Notification,
    visibility(state) { doc.visibilityState = state; doc.emit("visibilitychange"); },
    resolveSearch: (response) => searchGate.resolve(response),
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
  } }, deltaCursor("1"));
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

test("a ranked history result carries its epoch through the panel into one finite capture read", async (t) => {
  const f = fixture(t); await f.app.ready; await flush();
  f.get("history-search-open").emit("click");
  f.get("history-query").value = "needle";
  f.get("history-search-form").requestSubmit();
  await flush();
  const item = f.get("history-results").children[0];
  assert.equal(item.children[2].textContent, "Open captured segment");
  item.children[2].emit("click");
  const source = f.sources.at(-1);
  const cursor = deltaCursor("2");
  assert.equal(source.url.pathname, "/prefix/stream/deltas");
  assert.equal(source.url.searchParams.get("pane_id"), "7");
  assert.equal(source.url.searchParams.get("cursor"), cursor);
  assert.equal(source.url.searchParams.has("since_id"), false);
  source.message("ready", { schema: "ft.stream.v1", stream: "deltas", kind: "ready",
    data: { pane_id: 7, resume_after_id: 2, cursor } }, cursor);
  source.message("delta", { schema: "ft.stream.v1", stream: "deltas", kind: "delta",
    data: { pane_id: 7, segment_id: 3, seq: 0, captured_at: 1000, content_len: 6, content: "needle" } }, deltaCursor("3"));
  f.frame();
  assert.equal(f.get("pane-output-text").value, "needle");
  assert.match(f.get("pane-output-status").textContent, /Historical capture loaded/);
  assert.equal(source.closed, true);
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
  } }, deltaCursor("1"));
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

test("an initially hidden fleet starts no stream, snapshot, interval or painting loop", async (t) => {
  const f = fixture(t, { hidden: true }); await f.app.ready;
  assert.equal(f.sources.length, 0);
  assert.equal(f.requests.length, 0);
  assert.equal(f.frames.size, 0);
  assert.equal(f.intervals.size, 0);
  assert.equal(f.doc.documentElement.dataset.viewerReady, "suspended");
  assert.ok(f.renderers.every((renderer) => renderer.patches === 0 && renderer.renders === 0));
  f.host.emit("pageshow"); f.app.resume();
  assert.equal(f.frames.size, 0, "manual resume cannot override document visibility");
  f.visibility("visible"); f.visibility("visible"); await flush();
  assert.equal(f.fleetSources().length, 1);
  assert.equal(f.requests.length, 2);
  assert.equal(f.frames.size, 1);
  assert.equal(f.intervals.size, 1);
  assert.equal(f.doc.documentElement.dataset.viewerReady, "1");
  assert.ok(f.renderers[0].patches > 0);
});

test("hiding aborts snapshots and search, closes output and rejects late results", async (t) => {
  const f = fixture(t, { pendingSnapshots: true, pendingSearch: true }); await f.app.ready;
  f.openPane();
  const output = f.sources.at(-1);
  output.message("delta", { schema: "ft.stream.v1", stream: "deltas", kind: "delta", data: {
    pane_id: 7, segment_id: 9, seq: 2, content: "retained before hiding",
  } }, deltaCursor("9"));
  f.frame();
  f.get("history-search-open").emit("click");
  f.get("history-query").value = "needle";
  f.get("history-search-form").requestSubmit(); await flush();
  assert.equal(f.requests.length, 3);
  const model = f.viewers[0], paints = f.renderers[0].renders;
  f.visibility("hidden"); await flush();
  assert.ok(f.requests.every((request) => request.signal.aborted));
  assert.ok(f.sources.every((source) => source.closed));
  assert.equal(f.frames.size, 0);
  assert.equal(f.intervals.size, 0);
  assert.equal(f.timers.size, 0);
  assert.equal(f.doc.documentElement.dataset.viewerReady, "suspended");
  assert.equal(f.get("history-search").open, false);
  assert.equal(f.get("history-results").children.length, 0);
  output.message("delta", { schema: "ft.stream.v1", stream: "deltas", kind: "delta", data: {
    pane_id: 7, segment_id: 10, seq: 3, content: "late content must not be accepted",
  } }, deltaCursor("10"));
  f.resolveSearch(new Response(JSON.stringify({ ok: true, data: {
    results: [{ segment_id: 3, pane_id: 7, score: -0.5, captured_at: 1000, content_len: 6, snippet: "late needle", capture_cursor: deltaCursor("2") }], total: 1,
  } }))); await flush();
  assert.equal(f.get("pane-output-text").value, "retained before hiding");
  assert.equal(f.get("history-results").children.length, 0);
  assert.equal(f.renderers[0].renders, paints);
  assert.equal(model.bodies.length, 0);
  f.visibility("visible"); await flush();
  assert.equal(f.viewers[0], model);
  const resumedOutput = f.sources.filter((source) => !source.closed && source.url.pathname.endsWith("/stream/deltas"));
  assert.equal(resumedOutput.length, 1);
  assert.equal(resumedOutput[0].url.searchParams.get("cursor"), deltaCursor("9"));
  assert.equal(f.get("pane-output-text").value, "retained before hiding");
  assert.equal(f.frames.size, 1);
  assert.equal(f.intervals.size, 1);
});

test("visibility resume preserves an explicit pane pause and its received cursor", async (t) => {
  const f = fixture(t); await f.app.ready; await flush();
  f.openPane();
  f.sources.at(-1).message("delta", { schema: "ft.stream.v1", stream: "deltas", kind: "delta", data: {
    pane_id: 7, segment_id: 9, seq: 2, content: "paused output",
  } }, deltaCursor("9"));
  f.frame();
  f.get("pane-output-pause").emit("click");
  f.visibility("hidden"); f.visibility("visible"); await flush();
  assert.equal(f.sources.filter((source) => !source.closed && source.url.pathname.endsWith("/stream/deltas")).length, 0);
  assert.equal(f.get("pane-output-pause").textContent, "Resume");
  assert.equal(f.get("pane-output-text").value, "paused output");
  f.get("pane-output-pause").emit("click");
  assert.equal(f.sources.at(-1).url.searchParams.get("cursor"), deltaCursor("9"));
  assert.equal(f.sources.at(-1).closed, false);
});

test("queued old animation and refresh callbacks cannot create work after hide and show", async (t) => {
  const f = fixture(t); await f.app.ready; await flush();
  const staleFrame = [...f.frames.values()][0];
  const staleRefresh = [...f.intervals.values()][0].fn;
  f.visibility("hidden");
  staleFrame(); staleRefresh();
  assert.equal(f.frames.size, 0);
  f.visibility("visible"); await flush();
  const frames = [...f.frames.keys()], requests = f.requests.length;
  const renders = f.renderers[0].renders;
  staleFrame(); staleRefresh(); await flush();
  assert.deepEqual([...f.frames.keys()], frames);
  assert.equal(f.requests.length, requests);
  assert.equal(f.renderers[0].renders, renders);
  assert.equal(f.fleetSources().filter((source) => !source.closed).length, 1);
  assert.equal(f.intervals.size, 1);
  f.frame();
  assert.equal(f.frames.size, 1, "the resumed frame keeps sole scheduling ownership");
});

test("hidden import completion cannot paint and pagehide wins over a visibility event", async (t) => {
  const f = fixture(t, { pendingModel: true, pendingRenderer: true });
  f.visibility("hidden");
  f.resolveModel(); f.resolveRenderer(); await f.app.ready;
  assert.equal(f.requests.length, 0);
  assert.equal(f.sources.length, 0);
  assert.equal(f.frames.size, 0);
  assert.equal(f.doc.documentElement.dataset.viewerReady, "suspended");
  assert.ok(f.renderers.every((renderer) => renderer.patches === 0 && renderer.renders === 0));
  f.host.emit("pagehide");
  f.visibility("visible"); await flush();
  assert.equal(f.requests.length, 0);
  assert.equal(f.frames.size, 0);
  f.host.emit("pageshow"); await flush();
  assert.equal(f.fleetSources().length, 1);
  assert.equal(f.frames.size, 1);
  assert.ok(f.renderers[0].patches > 0);
});


const DETECTION_EPOCH = "0123456789abcdef0123456789abcdef";
function notificationReady(source) {
  const cursor = "event-v1:" + DETECTION_EPOCH + ":all:0";
  source.message("ready", { schema: "ft.stream.v1", stream: "events", kind: "ready",
    data: { channel: "all", pane_id: null, durable_detections: true, cursor } }, cursor);
}
function emitNotificationDetection(source, id = "1", paneUuid = "pane-original", { detectedAt = 1000, severity = "warning", eventType = "usage", sentAt = null } = {}) {
  source.emit("event", {
    data: '{"schema":"ft.stream.v1","stream":"events","kind":"event",'
      + (sentAt === null ? "" : '"ts_ms":' + JSON.stringify(sentAt) + ",")
      + '"data":{"delivery":"durable","detected_at":'
      + JSON.stringify(detectedAt) + ',"event":{"type":"pattern_detected","event_id":' + id + ',"pane_id":18446744073709551615,"pane_uuid":' + JSON.stringify(paneUuid)
      + ',"detection":{"severity":' + JSON.stringify(severity) + ',"event_type":' + JSON.stringify(eventType)
      + ',"matched_text":"secret terminal text"}}}}',
    lastEventId: "event-v1:" + DETECTION_EPOCH + ":all:" + id,
  });
}

test("explicit notifications retain only detections while hidden, then resume one foreground owner", async (t) => {
  const f = fixture(t, { notifications: true }); await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed); await flush();
  f.get("fleet-notifications-toggle").emit("click"); await flush();
  f.openPane();
  const pane = f.sources.at(-1);
  const paints = f.renderers[0].renders, requests = f.requests.length;
  f.visibility("hidden"); await flush();
  assert.equal(feed.closed, false);
  assert.equal(pane.closed, true, "notification opt-in never retains pane output");
  assert.equal(f.frames.size, 0);
  assert.equal(f.intervals.size, 0);
  const drains = f.viewers[0].drains;
  emitNotificationDetection(feed);
  assert.equal(f.notifications.length, 1);
  assert.ok(f.viewers[0].drains > drains, "hidden model queues remain bounded without rendering");
  assert.equal(f.renderers[0].renders, paints);
  assert.equal(f.requests.length, requests);
  assert.equal([...f.timers.values()].filter((timer) => timer.delay === 5000).length, 1);
  f.visibility("visible"); await flush();
  assert.equal(f.fleetSources().length, 1, "visibility does not replace the admitted detection stream");
  assert.equal(f.frames.size, 1);
  assert.equal(f.intervals.size, 1);
  f.get("fleet-notifications-toggle").emit("click");
  assert.equal(f.notifications[0].closed, true);
  f.visibility("hidden"); await flush();
  assert.equal(feed.closed, true);
  assert.equal(f.timers.size, 0);
});

test("permission revocation stops a hidden notification feed and closes existing alerts", async (t) => {
  const f = fixture(t, { notifications: true }); await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed); await flush();
  f.get("fleet-notifications-toggle").emit("click"); await flush();
  f.visibility("hidden");
  emitNotificationDetection(feed);
  const timer = [...f.timers].find(([, timer]) => timer.delay === 5000);
  f.Notification.permission = "denied";
  f.timers.delete(timer[0]); timer[1].fn();
  assert.equal(feed.closed, true);
  assert.equal(f.notifications[0].closed, true);
  assert.equal(f.timers.size, 0);
  assert.match(f.get("fleet-notifications-status").textContent, /permission was removed/);
});

test("notification click verifies session and exact pane identity without opening a terminal stream", async (t) => {
  let verified = 0;
  const f = fixture(t, { notifications: true,
    verifyNotificationSession: async () => { verified += 1; return true; },
    paneSnapshot: '{"ok":true,"data":{"panes":[{"pane_id":18446744073709551615,"pane_uuid":"pane-original"}]}}',
  }); await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed); await flush();
  f.get("fleet-notifications-toggle").emit("click"); await flush();
  emitNotificationDetection(feed);
  const sources = f.sources.length;
  f.notifications[0].onclick(); await flush();
  assert.equal(verified, 1);
  assert.equal(f.get("pane-output-id").value, "18446744073709551615");
  assert.equal(f.viewers[0].view, "detections");
  assert.equal(f.sources.length, sources, "inspection requires the manual Open output action");
  assert.match(f.get("fleet-notifications-status").textContent, /Choose Open output.*may no longer be live/);
});

test("notification clicks cannot resolve a reused pane or outlive session retirement", async (t) => {
  const verification = deferred();
  const f = fixture(t, { notifications: true, verifyNotificationSession: () => verification.promise,
    paneSnapshot: '{"ok":true,"data":{"panes":[{"pane_id":18446744073709551615,"pane_uuid":"reused-pane"}]}}',
  }); await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed); await flush();
  f.get("fleet-notifications-toggle").emit("click"); await flush();
  emitNotificationDetection(feed); f.notifications[0].onclick(); await flush();
  f.app.dispose(); const requests = f.requests.length;
  verification.resolve(true); await flush();
  assert.equal(f.requests.length, requests);
  assert.equal(f.get("pane-output-id").value, "");
  assert.equal(f.get("screen-text").textContent, "");

  const reused = fixture(t, { notifications: true,
    paneSnapshot: '{"ok":true,"data":{"panes":[{"pane_id":18446744073709551615,"pane_uuid":"reused-pane"}]}}',
  }); await reused.app.ready; await flush();
  const secondFeed = reused.fleetSources()[0]; notificationReady(secondFeed); await flush();
  reused.get("fleet-notifications-toggle").emit("click"); await flush();
  emitNotificationDetection(secondFeed); reused.notifications[0].onclick(); await flush();
  assert.equal(reused.get("pane-output-id").value, "");
  assert.match(reused.get("fleet-notifications-status").textContent, /identity changed/);
});

test("a notification click leaves pinned focus so its result can be seen", async (t) => {
  const f = fixture(t, { notifications: true,
    paneSnapshot: '{"ok":true,"data":{"panes":[{"pane_id":18446744073709551615,"pane_uuid":"pane-original"}]}}',
  }); await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed); await flush();
  f.get("fleet-notifications-toggle").emit("click"); await flush();
  f.openPane("7"); f.get("pane-output-pin").emit("click");
  pinnedTile(f).focus.emit("click");
  assert.equal(f.get("workspace").dataset.pinnedFocus, "true");
  emitNotificationDetection(feed); f.notifications[0].onclick(); await flush();
  assert.equal(f.get("workspace").dataset.pinnedFocus, "false", "focus mode hides the fleet view and the pane form");
  assert.match(f.get("fleet-notifications-status").textContent, /identity was verified/);
  assert.equal(f.get("pane-output-id").value, "18446744073709551615");
});

test("notification click refuses a detection the server could not attribute to a pane lifetime", async (t) => {
  // ft-ddxotx: durable frames carry pane_uuid null when the detection's
  // segment has no capture provenance (older events, or no segment at all).
  const f = fixture(t, { notifications: true, verifyNotificationSession: async () => true,
    paneSnapshot: '{"ok":true,"data":{"panes":[{"pane_id":18446744073709551615,"pane_uuid":"pane-original"}]}}',
  }); await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed); await flush();
  f.get("fleet-notifications-toggle").emit("click"); await flush();
  emitNotificationDetection(feed, "1", null);
  assert.equal(f.notifications.length, 1, "an unattributed detection still alerts");
  f.notifications[0].onclick(); await flush();
  assert.equal(f.get("pane-output-id").value, "");
  assert.match(f.get("fleet-notifications-status").textContent, /identity changed/);
});

test("hidden notification click waits for foreground session admission and pagehide closes it", async (t) => {
  let verified = 0;
  const f = fixture(t, { notifications: true, verifyNotificationSession: async () => { verified += 1; return true; },
    paneSnapshot: '{"ok":true,"data":{"panes":[{"pane_id":18446744073709551615,"pane_uuid":"pane-original"}]}}',
  }); await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed); await flush();
  f.get("fleet-notifications-toggle").emit("click"); await flush();
  f.visibility("hidden"); emitNotificationDetection(feed);
  f.notifications[0].onclick(); await flush();
  assert.equal(verified, 0);
  assert.equal(f.get("pane-output-id").value, "");
  f.visibility("visible"); await flush();
  assert.equal(verified, 1);
  assert.equal(f.get("pane-output-id").value, "18446744073709551615");
  f.host.emit("pagehide");
  assert.equal(feed.closed, true);
  assert.equal(f.timers.size, 0);
  assert.equal(f.get("fleet-notifications-toggle").attrs["aria-pressed"], "false");
});

test("attention triages admitted warnings and input requests without notification permission", async (t) => {
  let time = 10_000;
  const f = fixture(t, { now: () => time, paneSnapshot: '{"ok":true,"data":{"panes":[{"pane_id":18446744073709551615,"pane_uuid":"pane-original"}]}}' });
  await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed);
  const select = f.get("attention-items"), summary = f.get("attention-summary");
  emitNotificationDetection(feed, "9007199254740993");
  emitNotificationDetection(feed, "9007199254740993"); // already admitted
  emitNotificationDetection(feed, "9007199254740994", null, { severity: "info", eventType: "progress" });
  f.frame();
  assert.equal(select.children.length, 1);
  assert.match(select.children[0].textContent, /Unread · recent · warning.*#9007199254740993/);
  select.value = select.children[0].value;
  select.emit("change"); f.get("attention-review").emit("click");
  assert.equal(summary.textContent, "Attention (0 unread)");
  assert.match(select.children[0].textContent, /Reviewed/);
  emitNotificationDetection(feed, "9007199254740995", null, { severity: "info", eventType: "input.required" });
  time = 42_000; f.frame();
  assert.equal(select.children.length, 2);
  assert.match(select.children[1].textContent, /Reviewed · older/);
  assert.match(select.children[0].textContent, /input.required/);
  assert.equal(select.value, select.children[1].value, "insertion must preserve the exact reviewed event");
  assert.equal(summary.textContent, "Attention (1 unread)");
  assert.doesNotMatch(select.children.map((option) => option.textContent).join(" "), /secret/);
  f.get("attention-inspect").emit("click"); await flush();
  assert.match(f.get("attention-status").textContent, /identity was verified/);
  assert.equal(f.get("pane-output-id").value, "18446744073709551615");
  assert.equal(f.sources.length, 1);
  select.value = select.children[0].value; select.emit("change");
  assert.equal(f.get("attention-inspect").disabled, true);
  assert.equal(f.notifications.length, 0);
  f.app.dispose();
  assert.deepEqual(select.children, []);
});

test("attention ages a detection by server time, whether this browser's clock runs ahead or behind", async (t) => {
  for (const start of [50_000_000, 10]) {
    let time = start;
    const f = fixture(t, { now: () => time });
    await f.app.ready; await flush();
    const feed = f.fleetSources()[0]; notificationReady(feed);
    const select = f.get("attention-items");
    // Sent 1 s after detection: recent for 29 s more by this tab's clock.
    emitNotificationDetection(feed, "1", null, { detectedAt: 1_000_000, sentAt: 1_001_000 });
    // Sent a minute after detection, as a replayed backlog is: older on arrival.
    emitNotificationDetection(feed, "2", null, { detectedAt: 1_000_000, sentAt: 1_060_000 });
    f.frame();
    assert.match(select.children[1].textContent, /^Unread · recent · warning.* #1$/, "clock " + start);
    assert.match(select.children[0].textContent, /^Unread · older · warning.* #2$/, "clock " + start);
    time += 28_999; f.frame();
    assert.match(select.children[1].textContent, /recent/, "clock " + start);
    time += 1; f.frame();
    assert.match(select.children[1].textContent, /older/, "clock " + start);
    f.app.dispose();
  }
});

test("attention evicts a selected item without acting on its replacement", async (t) => {
  const verification = deferred();
  const f = fixture(t, { verifyNotificationSession: () => verification.promise });
  await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed);
  emitNotificationDetection(feed); f.frame();
  const list = f.get("attention-items");
  list.value = list.children[0].value; list.emit("change");
  f.get("attention-inspect").emit("click");
  const initialRequests = f.requests.length;
  for (let id = 2; id <= 51; id++) emitNotificationDetection(feed, String(id));
  f.frame();
  assert.equal(list.children.length, 50);
  assert.equal(list.value, "");
  assert.equal(f.get("attention-review").disabled, true);
  assert.match(f.get("attention-summary").textContent, /50 unread.*1 older item/);
  verification.resolve(true); await flush();
  assert.equal(f.requests.length, initialRequests);
  assert.equal(f.get("pane-output-id").value, "");
});

test("a notification state change leaves an attention verification running", async (t) => {
  const verification = deferred();
  const f = fixture(t, { notifications: true, verifyNotificationSession: () => verification.promise,
    paneSnapshot: '{"ok":true,"data":{"panes":[{"pane_id":18446744073709551615,"pane_uuid":"pane-original"}]}}',
  }); await f.app.ready; await flush();
  const feed = f.fleetSources()[0]; notificationReady(feed); await flush();
  emitNotificationDetection(feed); f.frame();
  const list = f.get("attention-items");
  list.value = list.children[0].value; list.emit("change");
  f.get("attention-inspect").emit("click");
  // Enabling publishes a requesting state while notifications are still off.
  f.get("fleet-notifications-toggle").emit("click"); await flush();
  assert.equal(f.get("fleet-notifications-toggle").attrs["aria-pressed"], "true");
  verification.resolve(true); await flush();
  assert.match(f.get("attention-status").textContent, /identity was verified/);
  assert.equal(f.get("pane-output-id").value, "18446744073709551615");
});

function pinnedTile(f, index = 0) {
  const panel = f.get("pinned-output-grid").children[index];
  const [controls, state, actions, warnings, transcript] = panel.children;
  return { panel, state, warnings, transcript,
    focus: controls.children[1], remove: controls.children[2],
    pause: actions.children[0], history: actions.children[1], follow: actions.children[2].children[0] };
}

function readyPinned(source, after = "0") {
  const pane = source.url.searchParams.get("pane_id");
  const cursor = deltaCursor(after, pane);
  source.emit("ready", {
    data: `{"schema":"ft.stream.v1","stream":"deltas","kind":"ready","data":{"pane_id":${pane},"resume_after_id":${after},"cursor":${JSON.stringify(cursor)}}}`,
    lastEventId: cursor,
  });
}

function pinnedDelta(source, id, content, sequence = id) {
  const pane = source.url.searchParams.get("pane_id");
  source.emit("delta", {
    data: `{"schema":"ft.stream.v1","stream":"deltas","kind":"delta","data":{"pane_id":${pane},"segment_id":${id},"seq":${sequence},"content":${JSON.stringify(content)}}}`,
    lastEventId: deltaCursor(id, pane),
  });
}

test("three pins compare exact independent panes while reserving an HTTP connection for session checks", async (t) => {
  const f = fixture(t);
  await f.app.ready; await flush();
  const pinned = [];
  for (const pane of ["7", "8", "18446744073709551615"]) {
    f.openPane(pane);
    f.get("pane-output-pin").emit("click");
    const source = f.sources.at(-1);
    pinned.push(source);
    assert.equal(source.url.searchParams.get("pane_id"), pane);
    assert.equal(source.url.pathname, "/prefix/stream/deltas");
    assert.equal(source.url.searchParams.has("since_id"), false);
    readyPinned(source);
    pinnedDelta(source, "9007199254740993", `<pane ${pane}>`, "0");
  }
  f.frame();
  assert.equal(f.get("pinned-output-title").textContent, "Pinned output (3/3)");
  assert.equal(f.get("workspace").dataset.pinsOpen, "true");
  assert.equal(f.get("pinned-output").hidden, false);
  for (const [index, pane] of ["7", "8", "18446744073709551615"].entries()) {
    assert.equal(pinnedTile(f, index).transcript.value, `<pane ${pane}>`);
    assert.equal(pinnedTile(f, index).transcript.readOnly, true);
  }
  const beforeDuplicate = f.sources.length;
  f.get("pane-output-pin").emit("click");
  assert.equal(f.sources.length, beforeDuplicate, "duplicate pin preserves its cursor and creates no subscription");
  assert.match(f.get("pinned-output-status").textContent, /already pinned/);
  f.openPane("9");
  const beforeCap = f.sources.length;
  f.get("pane-output-pin").emit("click");
  assert.equal(f.sources.length, beforeCap, "pin cap is checked before EventSource construction");
  assert.match(f.get("pinned-output-status").textContent, /Three views/);
  assert.equal(f.sources.filter((source) => !source.closed).length, 5,
    "three pins + one inspector + one fleet feed leave a browser HTTP/1 connection for ordinary requests");
  pinnedDelta(pinned[0], "9007199254740994", " first-only", "1"); f.frame();
  assert.equal(pinnedTile(f).transcript.value, "<pane 7> first-only");
  assert.equal(pinnedTile(f, 1).transcript.value, "<pane 8>");
  assert.equal(f.get("pane-output-id").value, "9", "later navigation cannot retarget pinned output");
});

test("pin pause, history replay, copy stability and Unicode bounds are independent", async (t) => {
  const f = fixture(t);
  await f.app.ready; await flush();
  f.openPane("7"); f.get("pane-output-pin").emit("click");
  const first = f.sources.at(-1); readyPinned(first); pinnedDelta(first, "1", "first diagnostic");
  f.openPane("8"); f.get("pane-output-pin").emit("click");
  const second = f.sources.at(-1); readyPinned(second); pinnedDelta(second, "2", "second diagnostic");
  f.frame();
  const a = pinnedTile(f), b = pinnedTile(f, 1);
  a.transcript.setSelectionRange(0, 5);
  // An already-admitted chunk may evict the selected prefix before the UI
  // sees the native selection event. The old displayed text must stay put.
  pinnedDelta(first, "3", "😀".repeat(80_000));
  f.frame();
  assert.equal(first.closed, true);
  assert.equal(a.transcript.value, "first diagnostic");
  assert.match(a.warnings.textContent, /selected text stable/);
  assert.equal(a.pause.textContent, "Resume");
  assert.equal(second.closed, false);
  pinnedDelta(second, "4", " continues"); f.frame();
  assert.equal(b.transcript.value, "second diagnostic continues");
  a.transcript.setSelectionRange(5, 5); a.transcript.emit("pointerup");
  assert.equal(a.transcript.value.length, 128 * 1024);
  assert.ok(a.transcript.value.startsWith("😀") && a.transcript.value.endsWith("😀"));
  assert.match(a.warnings.textContent, /discarded/);
  const beforeResume = f.sources.length;
  a.pause.emit("click");
  assert.equal(f.sources.length, beforeResume + 1);
  const resumed = f.sources.at(-1);
  assert.equal(resumed.url.searchParams.get("cursor"), deltaCursor("3"));
  assert.equal(resumed.url.searchParams.get("pane_id"), "7");
  readyPinned(resumed, "3");
  a.history.emit("click");
  assert.equal(resumed.closed, true);
  assert.equal(f.sources.at(-1).url.searchParams.get("since_id"), "0");
  assert.equal(f.sources.at(-1).url.searchParams.get("pane_id"), "7");
  assert.equal(a.transcript.value, "");
  assert.equal(b.transcript.value, "second diagnostic continues");
});

test("removing a pin retires its retry owner and stale callbacks cannot affect a replacement", async (t) => {
  const f = fixture(t);
  await f.app.ready; await flush();
  f.openPane("7"); f.get("pane-output-pin").emit("click");
  const source = f.sources.at(-1); readyPinned(source); pinnedDelta(source, "1", "private diagnostic"); f.frame();
  source.readyState = 2; source.emit("error");
  const pending = [...f.timers.values()].filter((timer) => timer.delay === 1_000);
  assert.equal(pending.length, 1);
  const retired = pinnedTile(f);
  retired.remove.emit("click");
  assert.equal(retired.transcript.value, "");
  assert.equal(f.get("pinned-output-grid").children.length, 0);
  assert.equal(f.get("workspace").dataset.pinsOpen, "false");
  f.get("pane-output-pin").emit("click");
  const replacement = f.sources.at(-1); readyPinned(replacement); pinnedDelta(replacement, "2", "replacement");
  const sourceCount = f.sources.length;
  pending[0].fn();
  pinnedDelta(source, "3", "late secret");
  retired.pause.emit("click"); retired.history.emit("click"); retired.focus.emit("click"); retired.remove.emit("click");
  f.frame();
  assert.equal(f.sources.length, sourceCount);
  assert.equal(pinnedTile(f).transcript.value, "replacement");
  assert.equal(replacement.closed, false);
  assert.equal(f.get("pinned-output-grid").children.length, 1);
});

test("hidden pages suspend every pin and session retirement erases all pinned data", async (t) => {
  const f = fixture(t, { notifications: true });
  await f.app.ready; await flush();
  notificationReady(f.fleetSources()[0]);
  f.get("fleet-notifications-toggle").emit("click"); await flush();
  const sources = [];
  for (const pane of ["7", "8"]) {
    f.openPane(pane); f.get("pane-output-pin").emit("click");
    const source = f.sources.at(-1); sources.push(source);
    readyPinned(source); pinnedDelta(source, pane, `private ${pane}`);
  }
  f.frame();
  const tiles = [pinnedTile(f), pinnedTile(f, 1)];
  tiles[0].pause.emit("click");
  f.visibility("hidden");
  assert.ok(sources.every((source) => source.closed));
  assert.equal(f.fleetSources()[0].closed, false, "only opted-in detections stay active in background");
  const hiddenCount = f.sources.length;
  f.get("pane-output-pin").emit("click");
  tiles[1].pause.emit("click"); tiles[1].history.emit("click");
  assert.equal(f.sources.length, hiddenCount);
  f.visibility("visible");
  const restored = f.sources.slice(hiddenCount).filter((source) => source.url.pathname.endsWith("/stream/deltas"));
  assert.equal(restored.length, 2, "one live pin plus inspector resume; explicitly paused pin stays paused");
  assert.ok(restored.some((source) => source.url.searchParams.get("cursor") === deltaCursor("8", "8")));
  assert.equal(tiles[0].pause.textContent, "Resume");
  pinnedDelta(sources[1], "9", "retired"); f.frame();
  assert.equal(tiles[1].transcript.value, "private 8");
  f.app.dispose();
  assert.equal(f.get("pinned-output-grid").children.length, 0);
  assert.ok(tiles.every((tile) => tile.transcript.value === ""));
  assert.ok(f.sources.every((source) => source.closed));
  await flush(); // The aborted fleet snapshot settles its deadline in finally.
  assert.equal(f.timers.size, 0);
  assert.equal(f.get("pane-output-pin").disabled, true);
  f.app.resume(); tiles[1].pause.emit("click"); f.get("pane-output-pin").emit("click");
  assert.equal(f.sources.length, hiddenCount + restored.length);
});

test("pin focus is a local layout and new inspection restores the full workspace", async (t) => {
  const f = fixture(t);
  await f.app.ready; await flush();
  for (const pane of ["7", "8"]) { f.openPane(pane); f.get("pane-output-pin").emit("click"); }
  const a = pinnedTile(f), b = pinnedTile(f, 1);
  const connections = f.sources.length;
  const requests = f.requests.length;
  const inputs = f.viewers[0].inputs.length;
  a.focus.emit("click");
  assert.equal(f.get("workspace").dataset.pinnedFocus, "true");
  assert.equal(a.panel.hidden, false); assert.equal(b.panel.hidden, true);
  assert.equal(a.focus.textContent, "Show all panes");
  assert.equal(a.panel.emit("keydown", { key: "Escape" }).prevented, true);
  assert.equal(b.panel.hidden, false);
  assert.equal(f.get("workspace").dataset.pinnedFocus, "false");
  assert.equal(f.sources.length, connections);
  assert.equal(f.requests.length, requests);
  assert.equal(f.viewers[0].inputs.length, inputs, "focusing capture tiles never sends terminal/model input");
  a.focus.emit("click"); f.openPane("9");
  assert.equal(f.get("workspace").dataset.pinnedFocus, "false");
  b.focus.emit("click"); b.remove.emit("click");
  assert.equal(a.panel.hidden, false);
  assert.equal(f.get("workspace").dataset.pinnedFocus, "false");
  f.get("pinned-output-clear").emit("click");
  assert.equal(f.get("pinned-output").hidden, true);
  assert.equal(f.sources.filter((source) => !source.closed).length, 2, "fleet and inspector retain ownership");
});

test("removing the last pin returns keyboard focus to a visible control after closing the inspector", async (t) => {
  const f = fixture(t);
  await f.app.ready; await flush();
  for (const action of ["remove", "clear"]) {
    f.openPane("7"); f.get("pane-output-pin").emit("click");
    const tile = pinnedTile(f);
    f.get("pane-output-close").emit("click");
    assert.equal(f.get("pane-output").hidden, true);
    if (action === "remove") tile.remove.emit("click");
    else f.get("pinned-output-clear").emit("click");
    assert.equal(f.get("pinned-output-grid").children.length, 0);
    assert.equal(f.doc.activeElement, f.get("pane-output-id"));
  }
});

test("historical pins read the exact capture once and cannot turn into live pane subscriptions", async (t) => {
  const f = fixture(t);
  await f.app.ready; await flush();
  f.get("history-search-open").emit("click");
  f.get("history-query").value = "needle";
  f.get("history-search-form").requestSubmit(); await flush();
  f.get("history-results").children[0].children[2].emit("click");
  const original = f.sources.at(-1);
  const beforeLoaded = f.sources.length;
  f.get("pane-output-pin").emit("click");
  assert.equal(f.sources.length, beforeLoaded, "unreceived historical captures are not pinnable");
  readyPinned(original, "2");
  const frame = { schema: "ft.stream.v1", stream: "deltas", kind: "delta",
    data: { pane_id: 7, segment_id: 3, seq: 0, captured_at: 1000, content_len: 6, content: "needle" } };
  original.message("delta", frame, deltaCursor("3")); f.frame();
  assert.equal(f.get("pane-output-pin").textContent, "Pin this capture");
  f.get("pane-output-pin").emit("click");
  const pinned = f.sources.at(-1);
  assert.equal(pinned.url.searchParams.get("cursor"), deltaCursor("2"));
  assert.equal(pinned.url.searchParams.has("since_id"), false);
  readyPinned(pinned, "2"); pinned.message("delta", frame, deltaCursor("3")); f.frame();
  const tile = pinnedTile(f);
  assert.equal(tile.transcript.value, "needle");
  assert.match(tile.state.textContent, /historical.*one historical capture/);
  assert.equal(tile.pause.disabled, true); assert.equal(tile.history.hidden, true);
  assert.equal(pinned.closed, true);
  const count = f.sources.length;
  tile.pause.emit("click"); tile.history.emit("click");
  pinnedDelta(pinned, "4", "later private capture"); f.frame();
  assert.equal(f.sources.length, count);
  assert.equal(tile.transcript.value, "needle");
  f.openPane("8"); f.get("pane-output-close").emit("click");
  assert.equal(tile.transcript.value, "needle");
  f.visibility("hidden"); f.visibility("visible");
  assert.equal(f.sources.filter((source) => !source.closed && source.url.pathname.endsWith("/stream/deltas")).length, 0);
});

test("a pinned capture interrupted by a hidden page or a failed read is read again", async (t) => {
  const f = fixture(t);
  await f.app.ready; await flush();
  f.get("history-search-open").emit("click");
  f.get("history-query").value = "needle";
  f.get("history-search-form").requestSubmit(); await flush();
  f.get("history-results").children[0].children[2].emit("click");
  const original = f.sources.at(-1);
  readyPinned(original, "2");
  const frame = { schema: "ft.stream.v1", stream: "deltas", kind: "delta",
    data: { pane_id: 7, segment_id: 3, seq: 0, captured_at: 1000, content_len: 6, content: "needle" } };
  original.message("delta", frame, deltaCursor("3")); f.frame();
  f.get("pane-output-pin").emit("click");
  const interrupted = f.sources.at(-1);
  f.visibility("hidden");
  assert.equal(interrupted.closed, true);
  f.visibility("visible"); await flush();
  const captureReads = () => f.sources.filter((source) => !source.closed
    && source.url.searchParams.get("cursor") === deltaCursor("2"));
  assert.equal(captureReads().length, 1, "restore reads the interrupted capture again");
  const [resumed] = captureReads();
  assert.notEqual(resumed, interrupted);
  // The tile has no retry control, so pinning the failed capture again retries it.
  resumed.emit("error", {}); f.frame();
  const tile = pinnedTile(f);
  assert.match(tile.state.textContent, /^unavailable/);
  f.get("pane-output-pin").emit("click");
  assert.match(f.get("pinned-output-status").textContent, /again/);
  assert.equal(captureReads().length, 1);
  const [retried] = captureReads();
  assert.notEqual(retried, resumed);
  readyPinned(retried, "2"); retried.message("delta", frame, deltaCursor("3")); f.frame();
  assert.equal(tile.transcript.value, "needle");
  assert.match(tile.state.textContent, /^historical/);
});
