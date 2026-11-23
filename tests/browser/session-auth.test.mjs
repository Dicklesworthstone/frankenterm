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

function fixture(t, initial = { required: true, authenticated: false }, { hidden = false } = {}) {
  const doc = new Events();
  doc.visibilityState = hidden ? "hidden" : "visible";
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
  let sequence = 0, time = 10_000;
  let responder = async (request) => request.method === "GET" ? reply(initial) : new Response("{}");
  const gate = mountAuthenticatedFleet({
    document: doc, window: host, now: () => time,
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
      const app = { options, disposals: 0, suspensions: 0, resumptions: 0,
        dispose() { this.disposals += 1; }, suspend(options) { this.suspensions += 1; this.suspendOptions = options; },
        resume() { this.resumptions += 1; } };
      apps.push(app);
      return app;
    },
    setTimer: (fn, delay) => { const id = ++sequence; timers.set(id, { fn, delay }); return id; },
    clearTimer: (id) => timers.delete(id),
  });
  t.after(() => gate.dispose());
  return { gate, doc, host, channels, requests, apps, sources, timers, get: doc.getElementById,
    reloads: () => reloads, respond: (fn) => { responder = fn; },
    setTime: (value) => { time = value; },
    visibility(state) { doc.visibilityState = state; doc.emit("visibilitychange"); },
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

test("an initially hidden tab waits for visibility before authentication or viewer startup", async (t) => {
  const f = fixture(t, authorized(), { hidden: true }); await f.gate.ready;
  assert.equal(f.requests.length, 0);
  assert.equal(f.apps.length, 0);
  assert.equal(f.timers.size, 0);
  f.host.emit("pageshow"); await flush();
  assert.equal(f.requests.length, 0, "pageshow alone must not activate a background tab");
  f.visibility("visible"); f.visibility("visible"); await flush();
  assert.equal(f.requests.length, 1);
  assert.equal(f.apps.length, 1);
  assert.equal(f.get("workspace").hidden, false);
});

test("visibility pauses once and resumes the same viewer only after same-epoch verification", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  const app = f.apps[0];
  const oldTimers = [...f.timers.values()].map((timer) => timer.fn);
  f.visibility("hidden"); f.visibility("hidden");
  assert.equal(app.suspensions, 1);
  assert.equal(app.disposals, 0);
  assert.equal(f.timers.size, 0);
  assert.equal(f.get("workspace").hidden, true);
  assert.equal(f.get("inspect-toolbar").hidden, true);
  const url = new URL("http://127.0.0.1:9999/panes");
  await assert.rejects(app.options.fetchImpl(url), /retired/);
  assert.throws(() => new app.options.EventSourceImpl(url), /retired/);

  const status = deferred(); f.respond(() => status.promise);
  f.visibility("visible"); f.host.emit("pageshow");
  assert.equal(app.resumptions, 0);
  assert.equal(f.get("workspace").hidden, true);
  assert.equal(f.requests.length, 2);
  await assert.rejects(app.options.fetchImpl(url), /retired/, "visibility is not auth admission");
  status.resolve(reply(authorized())); await flush();
  assert.equal(app.resumptions, 1);
  assert.equal(app.disposals, 0);
  assert.equal(f.apps.length, 1);
  assert.equal(f.reloads(), 0);
  assert.equal(f.get("workspace").hidden, false);
  for (const callback of oldTimers) callback();
  await flush();
  assert.equal(app.disposals, 0, "a queued old expiry cannot retire the resumed session");
  assert.equal(f.requests.length, 2, "a queued old poll cannot start background work");
  f.respond(async () => new Response("{}"));
  await app.options.fetchImpl(url);
  assert.equal(f.requests.at(-1).headers.get("x-ft-session-epoch"), authorized().session_epoch);
});

test("rapid hide and show fences an old status response from the new revalidation", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  const oldStatus = deferred(), newStatus = deferred();
  f.respond(() => oldStatus.promise);
  f.timer(15_000);
  const oldRequest = f.requests.at(-1);
  f.visibility("hidden");
  assert.equal(oldRequest.signal.aborted, true);
  f.respond(() => newStatus.promise);
  f.visibility("visible");
  const newRequest = f.requests.at(-1);
  oldStatus.resolve(reply({ required: true, authenticated: false })); await flush();
  assert.equal(f.apps[0].disposals, 0);
  assert.equal(f.apps[0].resumptions, 0);
  assert.equal(newRequest.signal.aborted, false);
  newStatus.resolve(reply(authorized())); await flush();
  assert.equal(f.apps[0].resumptions, 1);
  assert.equal(f.get("workspace").hidden, false);
});

test("old data replies and stream errors cannot relock or recheck a resumed session", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  const app = f.apps[0], oldReply = deferred();
  const source = new app.options.EventSourceImpl(new URL("http://127.0.0.1:9999/stream/events"));
  f.respond(() => oldReply.promise);
  const pending = app.options.fetchImpl(new URL("http://127.0.0.1:9999/panes"));
  const rejected = assert.rejects(pending, /retired/);
  f.visibility("hidden");
  f.respond(async () => reply(authorized()));
  f.visibility("visible"); await flush();
  const requests = f.requests.length;
  oldReply.resolve(new Response("old refusal", { status: 401 }));
  await rejected;
  source.emit("error"); await flush();
  assert.equal(app.disposals, 0);
  assert.equal(app.resumptions, 1);
  assert.equal(f.requests.length, requests);
  assert.equal(f.get("workspace").hidden, false);
});

test("changed, refused, expired or unverifiable grants never resume hidden captured data", async (t) => {
  for (const outcome of ["changed", "refused", "expired", "unavailable"]) {
    const f = fixture(t, authorized()); await f.gate.ready;
    f.visibility("hidden");
    if (outcome === "expired") f.setTime(authorized().expires_at_ms + 1);
    f.respond(async () => outcome === "unavailable" ? new Response("private proxy text", { status: 503 })
      : reply(outcome === "changed" ? { ...authorized(), session_epoch: "abcdef0123456789abcdef0123456789" }
        : outcome === "refused" ? { required: true, authenticated: false } : authorized()));
    f.visibility("visible"); await flush();
    assert.equal(f.apps[0].resumptions, 0, outcome);
    assert.equal(f.apps[0].disposals, 1, outcome);
    assert.equal(f.get("workspace").hidden, true, outcome);
    assert.equal(f.reloads(), outcome === "changed" ? 1 : 0, outcome);
    assert.doesNotMatch(f.get("web-auth-status").textContent, /private proxy text/);
  }
});

test("page retirement wins over visibility and pageshow cannot resume a still-hidden tab", async (t) => {
  for (const visibleFirst of [true, false]) {
    const f = fixture(t, authorized()); await f.gate.ready;
    f.visibility("hidden");
    f.host.emit("pagehide");
    assert.equal(f.apps[0].disposals, 1, "pagehide must retire even an already suspended app");
    const requests = f.requests.length;
    if (visibleFirst) f.visibility("visible");
    else f.host.emit("pageshow");
    await flush();
    assert.equal(f.requests.length, requests);
    assert.equal(f.reloads(), 0);
    if (visibleFirst) f.host.emit("pageshow");
    else f.visibility("visible");
    await flush();
    assert.equal(f.reloads(), 1);
    assert.equal(f.apps[0].resumptions, 0);
    assert.equal(f.apps.length, 1);
  }
});

test("late sign-in completion cannot overtake visibility revalidation", async (t) => {
  const f = fixture(t); await f.gate.ready;
  const login = deferred(); f.respond(() => login.promise);
  f.get("web-auth-token").value = "synthetic-operator-token-0123456789abcdef";
  f.get("web-auth-form").emit("submit");
  const request = f.requests.at(-1);
  f.visibility("hidden");
  assert.equal(request.signal.aborted, true);
  f.respond(async () => reply(authorized()));
  f.visibility("visible"); await flush();
  assert.equal(f.apps.length, 1);
  login.resolve(new Response("{}")); await flush();
  assert.equal(f.apps[0].disposals, 0);
  assert.equal(f.reloads(), 0);
  assert.deepEqual(f.channels[0].messages, []);
});

test("cross-tab revocation retires a hidden viewer before any visibility resume", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  f.visibility("hidden");
  const requests = f.requests.length;
  f.channels[0].emit("message", { data: { type: "signed-out" } });
  assert.equal(f.apps[0].disposals, 1);
  assert.equal(f.requests.length, requests);
  f.visibility("visible"); await flush();
  assert.equal(f.apps[0].resumptions, 0);
  assert.equal(f.reloads(), 1);
});


test("notification opt-in retains session expiry and revocation checks while rendering is suspended", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  const app = f.apps[0];
  app.options.onNotificationsChanged(true);
  assert.equal(app.options.notificationScope, "http://127.0.0.1:9999/|" + authorized().session_epoch);
  f.visibility("hidden");
  assert.deepEqual(app.suspendOptions, { keepNotifications: true });
  assert.equal(app.resumptions, 0);
  assert.equal(f.timers.size, 2, "expiry and status polling remain authoritative");
  f.timer(15_000); await flush();
  assert.equal(f.requests.length, 2);
  assert.equal(app.resumptions, 0, "a status poll cannot resume a hidden renderer");
  assert.equal(f.timers.size, 2);
  const fresh = deferred(); f.respond(() => fresh.promise);
  f.visibility("visible");
  assert.equal(app.resumptions, 0);
  fresh.resolve(reply(authorized())); await flush();
  assert.equal(app.resumptions, 1);
  assert.equal(f.apps.length, 1);
});

test("notification opt-out and revoked sessions close background ownership", async (t) => {
  for (const reason of ["disabled", "expired", "revoked", "pagehide"]) {
    const f = fixture(t, authorized()); await f.gate.ready;
    const app = f.apps[0]; app.options.onNotificationsChanged(true);
    f.visibility("hidden");
    if (reason === "disabled") app.options.onNotificationsChanged(false);
    if (reason === "expired") f.timer(3_600_000);
    if (reason === "revoked") {
      f.respond(async () => reply({ required: true, authenticated: false }));
      f.timer(15_000); await flush();
    }
    if (reason === "pagehide") f.host.emit("pagehide");
    assert.equal(f.timers.size, 0, reason);
    assert.equal(f.get("workspace").hidden, true, reason);
    if (reason === "disabled") {
      assert.equal(app.suspendOptions, undefined);
      assert.equal(app.disposals, 0);
    } else assert.equal(app.disposals, 1, reason);
    assert.equal(await app.options.verifyNotificationSession(), false, reason);
  }
});

test("notification inspection revalidates grant identity and fences stale logout callbacks", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  const app = f.apps[0];
  assert.equal(await app.options.verifyNotificationSession(), true);
  assert.equal(f.requests.length, 2);
  f.respond(async () => reply({ ...authorized(), session_epoch: "abcdef0123456789abcdef0123456789" }));
  assert.equal(await app.options.verifyNotificationSession(), false);
  assert.equal(app.disposals, 1);
  const requests = f.requests.length;
  assert.equal(await app.options.verifyNotificationSession(), false);
  assert.equal(f.requests.length, requests, "retired notification callbacks cannot issue requests");
});

test("an in-flight notification session check cannot survive sign-out", async (t) => {
  const f = fixture(t, authorized()); await f.gate.ready;
  const app = f.apps[0], pending = deferred();
  f.respond(() => pending.promise);
  const verification = app.options.verifyNotificationSession();
  f.respond(async () => new Response("{}"));
  f.get("web-auth-logout").emit("click");
  pending.resolve(reply(authorized()));
  assert.equal(await verification, false); await flush();
  assert.equal(app.disposals, 1);
  assert.equal(f.get("workspace").hidden, true);
});
