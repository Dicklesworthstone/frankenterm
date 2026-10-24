import test from "node:test";
import assert from "node:assert/strict";
import { createFleetStream } from "../../crates/frankenterm-browser-ui/web/app.mjs";

function fixture({ reject = () => false, throwConstruct = false } = {}) {
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
    onResync() { resyncs += 1; },
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
