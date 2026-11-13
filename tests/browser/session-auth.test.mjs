import test from "node:test";
import assert from "node:assert/strict";
import { mountAuthenticatedFleet } from "../../crates/frankenterm-browser-ui/web/app.mjs";

const flush = async () => { for (let i = 0; i < 80; i++) await Promise.resolve(); };
const deferred = () => {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
};
const reply = (data) => new Response(JSON.stringify({ ok: true, data }));
const authorized = () => ({ required: true, authenticated: true, expires_at_ms: 3_610_000,
  session_epoch: "1234567890abcdef1234567890abcdef" });

class Events {
  constructor() { this.listeners = new Map(); }
  addEventListener(type, listener) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(listener);
  }
  emit(type, fields = {}) {
    const event = { preventDefault() {}, ...fields };
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

function fixture(t, initial = { required: true, authenticated: false }) {
  const doc = new Events();
  doc.documentElement = { dataset: {} };
  const nodes = new Map();
  doc.getElementById = (id) => {
    if (!nodes.has(id)) {
      const node = new Events();
      Object.assign(node, { value: "", hidden: false, disabled: false, textContent: "", attrs: {},
        setAttribute(key, value) { this.attrs[key] = value; } });
      nodes.set(id, node);
    }
    return nodes.get(id);
  };
  const host = new Events();
  let reloads = 0;
  host.location = { href: "http://127.0.0.1:9999/ui/", reload() { reloads += 1; } };
  Object.defineProperty(host, "localStorage", { get() { throw new Error("do not store credentials"); } });
  Object.defineProperty(host, "sessionStorage", { get() { throw new Error("do not store credentials"); } });
  const channels = [];
  host.BroadcastChannel = class extends Events {
    constructor(name) { super(); this.name = name; this.messages = []; channels.push(this); }
    postMessage(message) { this.messages.push(message); }
    close() { this.closed = true; }
  };
  const requests = [], apps = [], sources = [], timers = new Map();
  let sequence = 0;
  let responder = async (request) => request.method === "GET" ? reply(initial) : new Response("{}");
  const gate = mountAuthenticatedFleet({
    document: doc, window: host, now: () => 10_000,
    EventSourceImpl: class extends Events {
      static CLOSED = 2;
      constructor(url) { super(); this.url = new URL(url); sources.push(this); }
    },
    fetchImpl: async (url, init = {}) => {
      const request = { url: new URL(url), method: "GET", ...init };
      requests.push(request);
      return responder(request);
    },
    mountApp: (options) => {
      const app = { options, disposals: 0, dispose() { this.disposals += 1; } };
      apps.push(app);
      return app;
    },
    setTimer: (fn, delay) => { const id = ++sequence; timers.set(id, { fn, delay }); return id; },
    clearTimer: (id) => timers.delete(id),
  });
  t.after(() => gate.dispose());
  return { gate, doc, host, channels, requests, apps, sources, timers, get: doc.getElementById,
    reloads: () => reloads, respond: (fn) => { responder = fn; },
    timer(delay) {
      const match = [...timers].find(([, timer]) => timer.delay === delay);
      assert.ok(match, `missing timer ${delay}`);
      const [id, timer] = match; timers.delete(id); timer.fn();
    },
  };
}

test("sign-in gates every viewer consumer and does not weaken missing status", async (t) => {
  const f = fixture(t); await f.gate.ready;
  assert.equal(f.apps.length, 0);
  assert.equal(f.get("workspace").hidden, true);
  assert.equal(f.get("inspect-toolbar").hidden, true);
  assert.equal(f.get("web-auth-form").hidden, false);
  assert.deepEqual(f.requests.map((request) => request.url.pathname), ["/auth/status"]);
  assert.equal(f.requests[0].credentials, "same-origin");
  assert.equal(f.requests[0].redirect, "error");
  f.respond(async () => new Response("private proxy error", { status: 404 }));
  f.get("web-auth-retry").emit("click"); await flush();
  assert.equal(f.apps.length, 0);
  assert.match(f.get("web-auth-status").textContent, /could not be verified/);
  assert.doesNotMatch(f.get("web-auth-status").textContent, /private proxy/);
});

test("an explicitly unauthenticated local server mounts once after status", async (t) => {
  const f = fixture(t, { required: false, authenticated: true }); await f.gate.ready;
  assert.equal(f.apps.length, 1);
  assert.equal(f.apps[0].options.managePageLifecycle, false);
  assert.equal(f.get("workspace").hidden, false);
  assert.equal(f.get("web-auth-controls").hidden, true);
  f.timer(15_000); await flush();
  assert.equal(f.apps.length, 1);
});

test("login keeps the credential out of URLs and clears the field before waiting", async (t) => {
  const f = fixture(t); await f.gate.ready;
  const sent = deferred();
  f.respond(() => sent.promise);
  const secret = "synthetic-operator-token-0123456789abcdef";
  f.get("web-auth-token").value = secret;
  f.get("web-auth-form").emit("submit");
  assert.equal(f.get("web-auth-token").value, "");
  assert.equal(f.get("web-auth-submit").disabled, true);
  assert.equal(f.apps.length, 0);
  const request = f.requests.at(-1);
  assert.equal(request.url.href, "http://127.0.0.1:9999/auth/session");
  assert.equal(request.method, "POST");
  assert.equal(request.headers["content-type"], "application/json");
  assert.equal(request.credentials, "same-origin");
  assert.deepEqual(JSON.parse(request.body), { token: secret });
  sent.resolve(new Response("ignored response")); await flush();
  assert.equal(f.reloads(), 1);
  assert.deepEqual(f.channels[0].messages, [{ type: "session-changed" }]);
  assert.equal(f.apps.length, 0, "a fresh page owns the new session");
});

test("invalid or refused credentials never reach viewer startup or diagnostics", async (t) => {
  const f = fixture(t); await f.gate.ready;
  f.get("web-auth-token").value = "short";
  f.get("web-auth-form").emit("submit");
  assert.equal(f.requests.length, 1);
  f.respond(async () => new Response("secret response should never be shown", { status: 401 }));
  f.get("web-auth-token").value = "synthetic-operator-token-0123456789abcdef";
  f.get("web-auth-form").emit("submit"); await flush();
  assert.equal(f.apps.length, 0);
  assert.equal(f.reloads(), 0);
  assert.equal(f.get("web-auth-token").value, "");
  assert.equal(f.get("web-auth-submit").disabled, false);
  assert.doesNotMatch(f.get("web-auth-status").textContent, /secret response|0123456789/);
});

test("logout retires captured data before its network request and broadcasts no identity", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  const sent = deferred(); f.respond(() => sent.promise);
  f.get("web-auth-logout").emit("click");
  assert.equal(f.apps[0].disposals, 1);
  assert.equal(f.get("workspace").hidden, true);
  assert.equal(f.requests.at(-1).method, "DELETE");
  assert.equal(f.requests.at(-1).body, undefined);
  sent.resolve(new Response("{}")); await flush();
  assert.deepEqual(f.channels[0].messages, [{ type: "signed-out" }]);
  assert.match(f.get("web-auth-status").textContent, /Signed out/);
});

test("expiry and cross-tab revocation retire the model instead of preserving its cursor", async (t) => {
  for (const trigger of ["expiry", "other-tab"]) {
    const f = fixture(t, authorized()); await f.gate.ready;
    if (trigger === "expiry") f.timer(3_600_000);
    else f.channels[0].emit("message", { data: { type: "signed-out" } });
    assert.equal(f.apps[0].disposals, 1);
    assert.equal(f.get("workspace").hidden, true);
    assert.equal(f.timers.size, 0);
    f.get("web-auth-retry").emit("click"); await flush();
    assert.equal(f.apps.length, 1);
    assert.equal(f.reloads(), 1, "reauthentication reloads instead of reusing the retired model");
  }
});

test("a data endpoint refusal immediately locks the active viewer", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  f.respond(async () => new Response("", { status: 401 }));
  await f.apps[0].options.fetchImpl(new URL("http://127.0.0.1:9999/panes"));
  assert.equal(f.apps[0].disposals, 1);
  assert.equal(f.get("workspace").hidden, true);
  assert.equal(f.get("web-auth-form").hidden, false);
});

test("cookie rotation cannot carry an existing model or stream cursor into a new grant", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  f.respond(async () => reply({ ...authorized(), session_epoch: "abcdef0123456789abcdef0123456789" }));
  f.timer(15_000); await flush();
  assert.equal(f.apps[0].disposals, 1);
  assert.equal(f.apps.length, 1);
  assert.equal(f.get("workspace").hidden, true);
  assert.equal(f.reloads(), 1);
});

test("reads and native stream retries remain bound to the original session epoch", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  const { fetchImpl, EventSourceImpl } = f.apps[0].options;
  await fetchImpl(new URL("http://127.0.0.1:9999/panes"), { headers: { accept: "application/json" } });
  assert.equal(f.requests.at(-1).headers.get("x-ft-session-epoch"), authorized().session_epoch);
  assert.equal(f.requests.at(-1).headers.get("accept"), "application/json");
  for (const path of ["stream/events?channel=all", "stream/deltas?pane_id=3"]) {
    const source = new EventSourceImpl(new URL(path, "http://127.0.0.1:9999/"));
    assert.equal(source.url.searchParams.get("session_epoch"), authorized().session_epoch);
    assert.equal(source.url.searchParams.get("token"), null);
  }
  assert.equal(EventSourceImpl.CLOSED, 2);
  f.respond(async () => reply({ ...authorized(), session_epoch: "abcdef0123456789abcdef0123456789" }));
  f.sources[0].emit("error"); await flush();
  assert.equal(f.apps[0].disposals, 1);
  assert.equal(f.reloads(), 1);
  await assert.rejects(fetchImpl(new URL("http://127.0.0.1:9999/panes")), /retired/);
  assert.throws(() => new EventSourceImpl(new URL("http://127.0.0.1:9999/stream/events")), /retired/);
});

test("late authentication and login responses cannot revive a hidden page", async (t) => {
  const f = fixture(t); await f.gate.ready;
  const sent = deferred(); f.respond(() => sent.promise);
  f.get("web-auth-token").value = "synthetic-operator-token-0123456789abcdef";
  f.get("web-auth-form").emit("submit");
  f.host.emit("pagehide");
  assert.equal(f.requests.at(-1).signal.aborted, true);
  sent.resolve(new Response("{}")); await flush();
  assert.equal(f.reloads(), 0);
  assert.equal(f.apps.length, 0);
  f.respond(async () => reply(authorized()));
  f.host.emit("pageshow"); await flush();
  assert.equal(f.apps.length, 1);
});

test("BFCache return checks the server before replacing a retired viewer", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  f.host.emit("pagehide");
  assert.equal(f.apps[0].disposals, 1);
  assert.equal(f.get("workspace").hidden, true);
  f.host.emit("pageshow"); await flush();
  assert.equal(f.reloads(), 1);
  assert.equal(f.apps.length, 1);
  f.gate.dispose(); f.host.emit("pageshow"); await flush();
  assert.equal(f.reloads(), 1, "disposed gate cannot resume");
});

test("malformed or oversized session status is refused, never a legacy-mode bypass", async (t) => {
  for (const data of [{ required: false, authenticated: false }, { required: true, authenticated: true },
    { required: true, authenticated: true, expires_at_ms: 9_999 }, { required: "false", authenticated: true }]) {
    const f = fixture(t, data); await f.gate.ready;
    assert.equal(f.apps.length, 0);
    assert.equal(f.get("workspace").hidden, true);
  }
  const f = fixture(t); await f.gate.ready;
  f.respond(async () => new Response("x".repeat(4097)));
  f.get("web-auth-retry").emit("click"); await flush();
  assert.equal(f.apps.length, 0);
  assert.match(f.get("web-auth-status").textContent, /could not be verified/);
});
