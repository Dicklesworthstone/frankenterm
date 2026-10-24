import test from "node:test";
import assert from "node:assert/strict";
import { createFleetStream, createFleetRefresh } from "../../crates/frankenterm-browser-ui/web/app.mjs";

function fixture({ reject = () => false, throwConstruct = false, resync = () => {} } = {}) {
  const sources = [];
  const timers = new Map();
  const allCallbacks = [];
  const delays = [];
  const frames = [];
  const errors = [];
  const states = [];
  let nextTimer = 1;
  let resyncs = 0;
  class Source {
    static CLOSED = 2;
    constructor(url) {
      if (throwConstruct) throw new Error("synthetic open failure");
      this.url = new URL(url);
      this.readyState = 0;
      this.listeners = new Map();
      this.closeCalls = 0;
      sources.push(this);
    }
    addEventListener(type, callback) {
      const callbacks = this.listeners.get(type) ?? [];
      callbacks.push(callback);
      this.listeners.set(type, callbacks);
    }
    close() { this.readyState = Source.CLOSED; this.closeCalls += 1; }
    emit(type, fields = {}) {
      for (const callback of this.listeners.get(type) ?? []) callback({ type, ...fields });
    }
  }
  const viewer = {
    ingestStreamFrame(data) {
      if (reject(data)) throw new Error("synthetic decode failure");
      frames.push(data);
    },
    reportError(message) { errors.push(message); },
    setStreamState(state) { states.push(state); },
  };
  const feed = createFleetStream(viewer, {
    apiRoot: new URL("https://example.test/ft/"),
    EventSourceImpl: Source,
    setTimer(callback, delay) {
      const id = nextTimer++;
      timers.set(id, callback);
      allCallbacks.push(callback);
      delays.push(delay);
      return id;
    },
    clearTimer(id) { timers.delete(id); },
    onResync() { resyncs += 1; return resync(); },
  });
  return {
    feed, sources, timers, allCallbacks, delays, frames, errors, states,
    get resyncs() { return resyncs; },
    runTimer() {
      assert.equal(timers.size, 1, "exactly one retry owner");
      const [id, callback] = timers.entries().next().value;
      timers.delete(id);
      callback();
    },
  };
}

function closed(source) {
  source.readyState = 2;
  source.emit("error");
}

function event(source, id, data = "accepted") {
  source.emit("event", { data, lastEventId: id });
}

test("start is idempotent and the first connection is live-only", () => {
  const f = fixture();
  f.feed.start();
  f.feed.start();
  assert.equal(f.sources.length, 1);
  assert.equal(f.sources[0].url.pathname, "/ft/stream/events");
  assert.equal(f.sources[0].url.searchParams.get("since_id"), null);
  assert.equal(f.sources[0].url.searchParams.get("channel"), "all");
  assert.deepEqual(f.states, [1]);
});

test("closed-source replacement resumes with the exact accepted i64 string", () => {
  const f = fixture();
  f.feed.start();
  event(f.sources[0], "9223372036854775807");
  closed(f.sources[0]);
  f.runTimer();
  assert.equal(f.sources[1].url.searchParams.get("since_id"), "9223372036854775807");
  assert.equal(f.sources[0].closeCalls, 1);
});

test("native transient reconnect retains one source and creates no replacement timer", () => {
  const f = fixture();
  f.feed.start();
  event(f.sources[0], "71");
  f.sources[0].emit("error");
  assert.equal(f.sources.length, 1);
  assert.equal(f.timers.size, 0);
  assert.equal(f.sources[0].closeCalls, 0);
  assert.equal(f.states.at(-1), 1);
});

test("late callbacks and repeated CLOSED errors cannot create duplicate streams", () => {
  const f = fixture();
  f.feed.start();
  const first = f.sources[0];
  event(first, "12");
  closed(first);
  closed(first);
  event(first, "99", "late");
  assert.equal(f.timers.size, 1);
  f.runTimer();
  assert.equal(f.sources.length, 2);
  assert.equal(f.sources[1].url.searchParams.get("since_id"), "12");
  assert.deepEqual(f.frames, ["accepted"]);
});

test("a rejected payload cannot advance the application resume cursor", () => {
  const f = fixture({ reject: (data) => data === "broken" });
  f.feed.start();
  event(f.sources[0], "9");
  event(f.sources[0], "10", "broken");
  f.runTimer();
  assert.equal(f.sources[1].url.searchParams.get("since_id"), "9");
  assert.match(f.errors.at(-1), /frame rejected/);
});

test("rejecting the first persisted frame uses an explicit recovery boundary", () => {
  const f = fixture({ reject: () => true });
  f.feed.start();
  event(f.sources[0], "10", "broken");
  f.runTimer();
  assert.equal(f.sources[1].url.searchParams.get("since_id"), "0");
});

test("named application errors are visible and do not acknowledge their inherited id", () => {
  const f = fixture();
  f.feed.start();
  event(f.sources[0], "15");
  f.sources[0].emit("error", {
    data: JSON.stringify({
      schema: "ft.stream.v1", kind: "error",
      data: { code: "event_replay_storage_error", message: "private backend detail" },
    }),
    lastEventId: "99",
  });
  assert.equal(f.errors.at(-1), "stream: event_replay_storage_error");
  assert.ok(f.errors.every((message) => !message.includes("private backend detail")));
  assert.equal(f.states.at(-1), 0);
  f.runTimer();
  assert.equal(f.sources[1].url.searchParams.get("since_id"), "15");
});

test("malformed or overflowing cursor ids fail closed without ingestion", () => {
  for (const id of ["-1", "1e2", "9223372036854775808", "1\n2", "delta-v1:all:9", "x".repeat(100)]) {
    const f = fixture();
    f.feed.start();
    event(f.sources[0], "7");
    event(f.sources[0], id, "bad cursor");
    f.runTimer();
    assert.equal(f.sources[1].url.searchParams.get("since_id"), "7", id);
    assert.deepEqual(f.frames, ["accepted"], id);
  }
});

test("ready and lag request snapshot recovery but cannot replace the event cursor", () => {
  const f = fixture();
  f.feed.start();
  event(f.sources[0], "22");
  f.sources[0].emit("ready", { data: "ready", lastEventId: "99" });
  f.sources[0].emit("lag", { data: "lag", lastEventId: "99" });
  assert.equal(f.resyncs, 2);
  closed(f.sources[0]);
  f.runTimer();
  assert.equal(f.sources[1].url.searchParams.get("since_id"), "22");
});

test("failed reconnects back off to a cap instead of resetting on TCP open or ready", () => {
  const f = fixture();
  f.feed.start();
  for (let i = 0; i < 7; i += 1) {
    const current = f.sources.at(-1);
    current.emit("open");
    current.emit("ready", { data: "ready" });
    closed(current);
    f.runTimer();
  }
  assert.deepEqual(f.delays, [1000, 2000, 4000, 8000, 16000, 30000, 30000]);
  event(f.sources.at(-1), "50");
  closed(f.sources.at(-1));
  assert.equal(f.delays.at(-1), 1000);
});

test("stop cancels retries and stale queued timer callbacks cannot revive an old generation", () => {
  const f = fixture();
  f.feed.start();
  event(f.sources[0], "81");
  closed(f.sources[0]);
  const lateTimer = f.allCallbacks[0];
  f.feed.stop();
  assert.equal(f.timers.size, 0);
  lateTimer();
  assert.equal(f.sources.length, 1);
  f.feed.start();
  lateTimer();
  assert.equal(f.sources.length, 2);
  assert.equal(f.sources[1].url.searchParams.get("since_id"), "81");
  f.feed.stop();
  event(f.sources[1], "999", "late after stop");
  assert.deepEqual(f.frames, ["accepted"]);
  assert.equal(f.sources[1].closeCalls, 1);
});

test("EventSource constructor failures remain retryable and cancellable", () => {
  const f = fixture({ throwConstruct: true });
  f.feed.start();
  assert.equal(f.timers.size, 1);
  assert.match(f.errors.at(-1), /could not be opened/);
  f.runTimer();
  assert.deepEqual(f.delays, [1000, 2000]);
  f.feed.stop();
  assert.equal(f.timers.size, 0);
});

test("server error diagnostics are finite for malformed, hostile and oversized payloads", () => {
  for (const data of ["not JSON", "x".repeat(4097), '{"schema":"ft.stream.v1","kind":"error","data":{"code":"secret\\nraw"}}']) {
    const f = fixture();
    f.feed.start();
    f.sources[0].emit("error", { data });
    assert.equal(f.errors.at(-1), "stream: invalid server error frame");
    assert.equal(f.timers.size, 1);
  }
});

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function streamPayload(type, fields = {}) {
  return JSON.stringify({ schema: "ft.stream.v1", kind: "event", data: { event: { type, ...fields } } });
}

function refreshFixture(limits = {}) {
  const requests = [];
  const timers = new Map();
  const errors = [];
  const applied = [];
  let nextTimer = 1;
  // This projection implements the inspected FleetViewer admission interface.
  // Tests execute the real host owner, not a reimplementation of its scheduler.
  // Rust decoding, rendering and real browser networking require separate proof.
  const viewer = {
    panes: [], events: [], lag: 0, gaps: new Set(), lastError: null, state: 0,
    ingestPanes(body) {
      const envelope = JSON.parse(body);
      if (!envelope.ok || !Array.isArray(envelope.data?.panes)) throw new Error("bad panes");
      this.panes = envelope.data.panes;
      this.lastError = null;
      applied.push("panes");
    },
    ingestEvents(body) {
      const envelope = JSON.parse(body);
      if (!envelope.ok || !Array.isArray(envelope.data?.events)) throw new Error("bad events");
      this.events = envelope.data.events;
      this.lastError = null;
      applied.push("events");
    },
    ingestStreamFrame(payload) {
      const frame = JSON.parse(payload);
      if (frame.schema !== "ft.stream.v1") throw new Error("bad stream");
      if (frame.kind === "ready") this.state = 2;
      if (frame.kind === "lag") this.lag += 1;
      if (frame.kind !== "event") return;
      const event = frame.data.event;
      if (event.type === "pattern_detected") {
        const id = event.event_id ?? null;
        if (id === null || !this.events.some((row) => row.id === id)) {
          this.events.unshift({ id, label: event.label });
        }
      } else if (event.type === "pane_discovered") {
        this.panes = this.panes.filter((pane) => pane.pane_id !== event.pane_id);
        this.panes.push({ pane_id: event.pane_id, title: event.title });
      } else if (event.type === "pane_disappeared") {
        this.panes = this.panes.filter((pane) => pane.pane_id !== event.pane_id);
      } else if (event.type === "gap_detected") {
        this.gaps.add(event.pane_id);
      }
    },
    reportError(message) { this.lastError = message; errors.push(message); },
    setStreamState(state) { this.state = state; },
  };
  const snapshots = createFleetRefresh(viewer, {
    apiRoot: new URL("https://example.test/ft/"),
    fetchImpl(url, options) {
      const request = { ...deferred(), url, options };
      requests.push(request);
      return request.promise;
    },
    setTimer(callback, delay) {
      const id = nextTimer++;
      timers.set(id, { callback, delay });
      return id;
    },
    clearTimer(id) { timers.delete(id); },
    ...limits,
  });
  snapshots.start();
  return { snapshots, viewer, requests, timers, errors, applied };
}

function jsonResponse(value) {
  return new Response(JSON.stringify(value), { headers: { "content-type": "application/json" } });
}

function replyPair(f, offset = 0, panes = [], events = []) {
  f.requests[offset].resolve(jsonResponse({ ok: true, data: { panes } }));
  f.requests[offset + 1].resolve(jsonResponse({ ok: true, data: { events } }));
}

test("history refresh cannot erase a detection admitted while the fetch was pending", async () => {
  const f = refreshFixture();
  const pending = f.snapshots.refresh();
  f.snapshots.ingestStreamFrame(streamPayload("pattern_detected", { event_id: 2, label: "new" }));
  assert.deepEqual(f.viewer.events, [{ id: 2, label: "new" }], "live delivery is not delayed");
  replyPair(f, 0, [], [{ id: 1, label: "old" }]);
  assert.equal(await pending, true);
  assert.deepEqual(f.viewer.events.map((row) => row.id), [2, 1]);
  assert.equal(f.timers.size, 0);
});

test("pane disappearance and discovery win over an older inventory snapshot", async () => {
  const f = refreshFixture();
  const pending = f.snapshots.refresh();
  f.snapshots.ingestStreamFrame(streamPayload("pane_disappeared", { pane_id: 7 }));
  f.snapshots.ingestStreamFrame(streamPayload("pane_discovered", { pane_id: 8, title: "new pane" }));
  replyPair(f, 0, [{ pane_id: 7, title: "closed pane" }]);
  await pending;
  assert.deepEqual(f.viewer.panes, [{ pane_id: 8, title: "new pane" }]);
});

test("partial snapshot failure preserves live idless events without replaying them twice", async () => {
  const f = refreshFixture();
  const pending = f.snapshots.refresh();
  f.snapshots.ingestStreamFrame(streamPayload("pattern_detected", { label: "ephemeral" }));
  f.snapshots.ingestStreamFrame(streamPayload("pane_discovered", { pane_id: 8, title: "live" }));
  f.requests[0].resolve(jsonResponse({ ok: true, data: { panes: [] } }));
  f.requests[1].resolve(jsonResponse({ ok: false, error: "synthetic failure" }));
  assert.equal(await pending, false);
  assert.deepEqual(f.viewer.events, [{ id: null, label: "ephemeral" }]);
  assert.equal(f.viewer.panes[0].pane_id, 8);
  assert.match(f.viewer.lastError, /events: snapshot rejected/);
});

test("a successful sibling endpoint cannot hide a failed pane refresh", async () => {
  const f = refreshFixture();
  const pending = f.snapshots.refresh();
  f.requests[0].reject(new Error("private backend detail"));
  f.requests[1].resolve(jsonResponse({ ok: true, data: { events: [{ id: 1 }] } }));
  assert.equal(await pending, false);
  assert.deepEqual(f.viewer.events, [{ id: 1 }]);
  assert.equal(f.viewer.lastError, "panes: network request failed");
  assert.ok(f.errors.every((error) => !error.includes("private backend detail")));
});

test("lag and gap diagnostics are not counted again during snapshot reconciliation", async () => {
  const f = refreshFixture();
  const pending = f.snapshots.refresh();
  f.snapshots.ingestStreamFrame(JSON.stringify({ schema: "ft.stream.v1", kind: "lag" }));
  f.snapshots.ingestStreamFrame(streamPayload("gap_detected", { pane_id: 3 }));
  replyPair(f);
  await pending;
  assert.equal(f.viewer.lag, 1);
  assert.deepEqual([...f.viewer.gaps], [3]);
});

test("concurrent refresh requests coalesce into one in-flight pair and one followup", async () => {
  const f = refreshFixture();
  const first = f.snapshots.refresh();
  for (let i = 0; i < 30; i += 1) assert.equal(f.snapshots.refresh(), first);
  assert.equal(f.requests.length, 2);
  replyPair(f);
  await first;
  assert.equal(f.requests.length, 4);
  replyPair(f, 2);
  // Let the second pair settle without requesting a third refresh.
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(f.requests.length, 4);
  assert.equal(f.timers.size, 0);
});

test("stop aborts the old generation; late responses cannot overwrite a restarted viewer", async () => {
  const f = refreshFixture();
  const old = f.snapshots.refresh();
  f.snapshots.stop();
  assert.ok(f.requests.slice(0, 2).every((request) => request.options.signal.aborted));
  f.snapshots.start();
  const current = f.snapshots.refresh();
  replyPair(f, 2, [{ pane_id: 2 }], [{ id: 2 }]);
  await current;
  replyPair(f, 0, [{ pane_id: 1 }], [{ id: 1 }]);
  assert.equal(await old, false);
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(f.viewer.panes, [{ pane_id: 2 }]);
  assert.deepEqual(f.viewer.events, [{ id: 2 }]);
  assert.deepEqual(f.applied, ["panes", "events"]);
  assert.equal(f.errors.length, 0);
});

test("deadline releases a hung refresh even if the transport ignores cancellation", async () => {
  const f = refreshFixture();
  const pending = f.snapshots.refresh();
  assert.equal(f.timers.size, 1);
  const deadline = f.timers.values().next().value;
  assert.equal(deadline.delay, 10_000);
  deadline.callback();
  assert.equal(await pending, false);
  assert.ok(f.requests.every((request) => request.options.signal.aborted));
  assert.match(f.viewer.lastError, /timed out/);
  const retry = f.snapshots.refresh();
  replyPair(f, 2, [{ pane_id: 4 }]);
  assert.equal(await retry, true);
  assert.deepEqual(f.viewer.panes, [{ pane_id: 4 }]);
});

test("frame-cap overflow discards only the stale snapshot, not accepted live detections", async () => {
  const f = refreshFixture({ journalLimitFrames: 2 });
  f.viewer.events = [{ id: 1 }];
  const pending = f.snapshots.refresh();
  for (const id of [2, 3, 4]) {
    f.snapshots.ingestStreamFrame(streamPayload("pattern_detected", { event_id: id }));
  }
  replyPair(f, 0, [{ pane_id: 5 }], [{ id: 1 }]);
  assert.equal(await pending, false);
  assert.deepEqual(f.viewer.events.map((row) => row.id), [4, 3, 2, 1]);
  assert.deepEqual(f.viewer.panes, [{ pane_id: 5 }]);
  assert.match(f.viewer.lastError, /events: snapshot superseded/);
  assert.deepEqual(f.applied, ["panes"]);
});

test("byte-cap overflow protects the inventory while the independent history still refreshes", async () => {
  const f = refreshFixture({ journalLimitBytes: 64 });
  const pending = f.snapshots.refresh();
  f.snapshots.ingestStreamFrame(streamPayload("pane_discovered", { pane_id: 9, title: "a live pane" }));
  replyPair(f, 0, [], [{ id: 8 }]);
  assert.equal(await pending, false);
  assert.equal(f.viewer.panes[0].pane_id, 9);
  assert.deepEqual(f.viewer.events, [{ id: 8 }]);
  assert.match(f.viewer.lastError, /panes: snapshot superseded/);
});

test("stream diagnostics survive successful background refresh until a valid stream frame arrives", async () => {
  const f = refreshFixture();
  let pending = f.snapshots.refresh();
  f.snapshots.reportError("stream: event_replay_storage_error");
  replyPair(f);
  await pending;
  assert.equal(f.viewer.lastError, "stream: event_replay_storage_error");
  f.snapshots.ingestStreamFrame(JSON.stringify({ schema: "ft.stream.v1", kind: "ready" }));
  pending = f.snapshots.refresh();
  replyPair(f, 2);
  assert.equal(await pending, true);
  assert.equal(f.viewer.lastError, null);
});

test("HTTP failure is not accepted as successful data even when its JSON claims ok", async () => {
  const f = refreshFixture();
  const pending = f.snapshots.refresh();
  f.requests[0].resolve(new Response('{"ok":true,"data":{"panes":[]}}', { status: 503 }));
  f.requests[1].resolve(jsonResponse({ ok: true, data: { events: [] } }));
  assert.equal(await pending, false);
  assert.match(f.viewer.lastError, /panes: HTTP 503/);
  assert.deepEqual(f.applied, ["events"]);
});

test("declared oversized response is cancelled before its body is materialized", async () => {
  const f = refreshFixture({ bodyLimitBytes: 128 });
  let cancelled = false;
  const body = new ReadableStream({ cancel() { cancelled = true; } });
  const pending = f.snapshots.refresh();
  f.requests[0].resolve(new Response(body, { headers: { "content-length": "129" } }));
  f.requests[1].resolve(jsonResponse({ ok: true, data: { events: [] } }));
  assert.equal(await pending, false);
  assert.equal(cancelled, true);
  assert.match(f.viewer.lastError, /byte limit/);
});

test("streaming byte limit catches an oversized body without a content-length header", async () => {
  const f = refreshFixture({ bodyLimitBytes: 128 });
  let cancelled = false;
  let chunks = 0;
  const body = new ReadableStream({
    pull(controller) { chunks += 1; controller.enqueue(new Uint8Array(80).fill(65)); },
    cancel() { cancelled = true; },
  });
  const pending = f.snapshots.refresh();
  f.requests[0].resolve(new Response(body));
  f.requests[1].resolve(jsonResponse({ ok: true, data: { events: [] } }));
  assert.equal(await pending, false);
  assert.equal(cancelled, true);
  assert.ok(chunks <= 4, `bounded reads, got ${chunks}`);
  assert.match(f.viewer.lastError, /byte limit/);
});

test("incremental UTF-8 decoding preserves multibyte characters split across chunks", async () => {
  const f = refreshFixture();
  const bytes = new TextEncoder().encode(JSON.stringify({ ok: true, data: { panes: [{ pane_id: 1, title: "雪🚀" }] } }));
  let index = 0;
  const body = new ReadableStream({
    pull(controller) {
      if (index === bytes.length) controller.close();
      else controller.enqueue(bytes.slice(index, ++index));
    },
  });
  const pending = f.snapshots.refresh();
  f.requests[0].resolve(new Response(body));
  f.requests[1].resolve(jsonResponse({ ok: true, data: { events: [] } }));
  assert.equal(await pending, true);
  assert.equal(f.viewer.panes[0].title, "雪🚀");
});

test("invalid UTF-8 is a visible read failure, not silently replaced input", async () => {
  const f = refreshFixture();
  const pending = f.snapshots.refresh();
  f.requests[0].resolve(new Response(new Uint8Array([0xff])));
  f.requests[1].resolve(jsonResponse({ ok: true, data: { events: [] } }));
  assert.equal(await pending, false);
  assert.equal(f.viewer.lastError, "panes: response could not be read");
});

test("stale resync promise rejection cannot contaminate a new stream generation", async () => {
  const recovery = deferred();
  const f = fixture({ resync: () => recovery.promise });
  f.feed.start();
  f.sources[0].emit("ready", { data: "ready" });
  f.feed.stop();
  f.feed.start();
  recovery.reject(new Error("old generation"));
  await Promise.resolve();
  assert.deepEqual(f.errors, []);
});

test("snapshot limits reject zero, negative, non-finite and fractional values", () => {
  for (const timeoutMs of [0, -1, NaN, Infinity, 0.5]) {
    assert.throws(() => refreshFixture({ timeoutMs }), /invalid snapshot limit/);
  }
});

test("a detection present in both the snapshot and live journal remains unique", async () => {
  const f = refreshFixture();
  const pending = f.snapshots.refresh();
  f.snapshots.ingestStreamFrame(streamPayload("pattern_detected", { event_id: 2, label: "new" }));
  replyPair(f, 0, [], [{ id: 2, label: "new" }, { id: 1, label: "old" }]);
  await pending;
  assert.deepEqual(f.viewer.events.map((row) => row.id), [2, 1]);
});

test("deadline cancels a body stalled after headers while preserving the successful sibling", async () => {
  const f = refreshFixture();
  let cancelled = false;
  let started;
  const reading = new Promise((resolve) => { started = resolve; });
  const body = new ReadableStream({
    pull() { started(); },
    cancel() { cancelled = true; },
  });
  const pending = f.snapshots.refresh();
  f.requests[0].resolve(new Response(body));
  f.requests[1].resolve(jsonResponse({ ok: true, data: { events: [{ id: 9 }] } }));
  await reading;
  await new Promise((resolve) => setImmediate(resolve));
  f.timers.values().next().value.callback();
  assert.equal(await pending, false);
  assert.equal(cancelled, true);
  assert.deepEqual(f.viewer.events, [{ id: 9 }]);
  assert.match(f.viewer.lastError, /panes: request cancelled or timed out/);
});

test("body size exactly at the cap remains readable", async () => {
  const value = { ok: true, data: { panes: [{ pane_id: 1 }] } };
  const size = new TextEncoder().encode(JSON.stringify(value)).length;
  const f = refreshFixture({ bodyLimitBytes: size });
  const pending = f.snapshots.refresh();
  f.requests[0].resolve(jsonResponse(value));
  f.requests[1].resolve(jsonResponse({ ok: true, data: { events: [] } }));
  assert.equal(await pending, true);
  assert.deepEqual(f.viewer.panes, [{ pane_id: 1 }]);
});

test("integrated stream and refresh owners retain live data and its acknowledged reconnect cursor", async () => {
  const f = refreshFixture();
  const sources = [];
  const retryCallbacks = [];
  class Source {
    static CLOSED = 2;
    constructor(url) { this.url = new URL(url); this.listeners = new Map(); sources.push(this); }
    addEventListener(type, callback) { this.listeners.set(type, callback); }
    close() { this.readyState = 2; }
    emit(type, fields = {}) { this.listeners.get(type)?.({ type, ...fields }); }
  }
  const stream = createFleetStream(f.snapshots, {
    apiRoot: new URL("https://example.test/ft/"), EventSourceImpl: Source,
    setTimer(callback) { retryCallbacks.push(callback); return retryCallbacks.length; },
    clearTimer() {}, onResync: f.snapshots.refresh,
  });
  stream.start();
  const pending = f.snapshots.refresh();
  sources[0].emit("event", {
    data: streamPayload("pattern_detected", { event_id: 2, label: "new" }), lastEventId: "2",
  });
  sources[0].emit("error", {
    data: JSON.stringify({ schema: "ft.stream.v1", kind: "error", data: { code: "event_replay_storage_error" } }),
    lastEventId: "99",
  });
  replyPair(f, 0, [], [{ id: 1, label: "old" }]);
  await pending;
  assert.deepEqual(f.viewer.events.map((row) => row.id), [2, 1]);
  assert.match(f.viewer.lastError, /event_replay_storage_error/);
  assert.equal(f.viewer.state, 0);
  assert.equal(retryCallbacks.length, 1);
  retryCallbacks[0]();
  assert.equal(sources[1].url.searchParams.get("since_id"), "2");
  stream.stop();
  f.snapshots.stop();
});
