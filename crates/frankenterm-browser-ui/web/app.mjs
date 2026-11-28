// FrankenTerm browser fleet viewer host.
//
// This page owns all network I/O and DOM events. The WASM FleetViewer owns
// state and renders frames; FrankenTermWeb paints their patches. The viewer is
// read-only: nothing here writes to a pane.
import { createPaneOutput, decodeFrame, mountPaneOutput, mountPinnedOutputs } from "./pane-output.mjs";
import { mountHistorySearch } from "./history-panel.mjs";

const REFRESH_MS = 15_000;
const CLOCK_MS = 1_000;
const STREAM_RETRY_MAX_MS = 30_000;
const STREAM_FRAME_MAX_CHARS = 8 * 1024 * 1024;
const STREAM = { DISCONNECTED: 0, CONNECTING: 1, LIVE: 2 };
const FORWARDED_NAV_KEYS = new Set(["ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown"]);
const TEXT_NAV_KEYS = new Set(["ArrowUp", "ArrowDown", "Home", "End", "j", "k", "g", "G", "1", "2", "r"]);
const GRID_MAX_COLS = 240;
const GRID_MAX_ROWS = 100;
const SCREEN_TEXT_MAX_CHARS = 2 * GRID_MAX_COLS * GRID_MAX_ROWS + GRID_MAX_ROWS;

const NOTIFICATION_INTERVAL_MS = 5_000;
const NOTIFICATION_CURSOR_KEY = "frankenterm-detection-notifications-v1";

function needsAttention(target) {
  return ["warning", "error", "critical"].includes(target.severity)
    || ["auth.device_code", "auth.error", "auth.login_required", "auth.oauth_required",
      "session.approval_needed", "auth.prompt", "prompt", "prompt.waiting",
      "approval.required", "permission.required", "input.required", "waiting_for_input"].includes(target.eventType);
}

// The SSE owner already deduplicates durable events by epoch and exact event ID.
// Keep only the small display projection in tab memory. Never persist a review
// or mark the underlying watcher event handled.
function mountAttentionQueue(doc, inspect, now) {
  const limit = 50;
  const list = doc.getElementById("attention-items");
  const summary = doc.getElementById("attention-summary");
  const review = doc.getElementById("attention-review");
  const inspectButton = doc.getElementById("attention-inspect");
  const captureButton = doc.getElementById("attention-capture");
  const status = doc.getElementById("attention-status");
  let items = [];
  let dropped = 0;
  let dirty = true;
  let staleBoundary = Infinity;
  const selected = () => items.find((row) => row.key === list.value);
  function buttons() {
    const row = selected();
    review.disabled = !row;
    review.textContent = row?.reviewed ? "Mark unread" : "Mark reviewed";
    inspectButton.disabled = !row?.target.paneUuid;
    captureButton.disabled = !/^[0-9a-f]{32}$/.test(row?.target.paneUuid ?? "");
  }
  function render() {
    const time = now();
    if (!dirty && time < staleBoundary) return;
    dirty = false;
    staleBoundary = Infinity;
    const previous = list.value;
    const options = items.map((row) => {
      const option = doc.createElement("option");
      option.value = row.key;
      const { severity, paneId, eventType } = row.target;
      const expiresAt = row.detectedAt === null ? null : row.detectedAt + 30_000;
      if (expiresAt !== null && expiresAt > time) staleBoundary = Math.min(staleBoundary, expiresAt);
      const freshness = expiresAt === null ? "time unknown" : time >= expiresAt ? "older" : "recent";
      option.textContent = (row.reviewed ? "Reviewed" : "Unread") + " · " + freshness + " · "
        + (severity || "attention") + (eventType && /^[a-zA-Z0-9._-]{1,80}$/.test(eventType) ? " · " + eventType : "")
        + " · pane " + paneId + " · #" + row.target.id;
      return option;
    });
    list.replaceChildren(...options);
    // Reordering, eviction and replay must never retarget the user's action.
    list.value = items.some((row) => row.key === previous) ? previous : "";
    list.disabled = items.length === 0;
    const unread = items.filter((row) => !row.reviewed).length;
    summary.textContent = "Attention (" + unread + " unread)" + (dropped ? " · " + dropped + " older items omitted" : "");
    buttons();
  }
  list.addEventListener("change", () => { status.textContent = ""; buttons(); });
  review.addEventListener("click", () => {
    const row = selected();
    if (!row) return;
    row.reviewed = !row.reviewed;
    dirty = true;
    render();
  });
  inspectButton.addEventListener("click", () => {
    const row = selected();
    if (row?.target.paneUuid) inspect(row.target);
  });
  captureButton.addEventListener("click", () => {
    const row = selected();
    if (row && !captureButton.disabled) inspect(row.target, true);
  });
  return {
    // detectedAt and sentAt (the frame's ts_ms) are both server wall-clock
    // times. Their difference is the item's age when the server sent it, which
    // this tab's clock can count on from arrival, so a browser clock that runs
    // ahead or behind the server's no longer mislabels the item.
    detection(target, detectedAt, sentAt = null) {
      if (!needsAttention(target)) return;
      const key = target.epoch + ":" + target.id;
      const localDetectedAt = detectedAt === null || sentAt === null
        ? detectedAt : now() - Math.max(0, sentAt - detectedAt);
      items.unshift({ key, target, detectedAt: localDetectedAt, reviewed: false });
      if (items.length > limit) { items.pop(); dropped += 1; }
      dirty = true;
    },
    contains(target) { return selected()?.target === target; },
    report(message) { status.textContent = message; },
    render,
    clear() { items = []; dropped = 0; dirty = true; status.textContent = ""; render(); },
  };
}

// Explicit, per-tab opt-in. Persist only bounded cursor identities, never an
// event payload, pane title, terminal text, access token or notification target.
export function createDetectionNotifications({
  NotificationImpl = globalThis.Notification,
  storage = null,
  scope,
  setTimer = globalThis.setTimeout,
  clearTimer = globalThis.clearTimeout,
  now = Date.now,
  onChange = () => {},
  onInspect = () => {},
} = {}) {
  const supported = typeof NotificationImpl === "function"
    && typeof NotificationImpl.requestPermission === "function";
  const persistentScope = typeof scope === "string" && scope.length <= 2048 ? scope : null;
  let enabled = false;
  let requesting = false;
  let disposed = false;
  let generation = 0;
  let timer = null;
  let current = null;
  let pending = null;
  let highWater = null;
  let lastDisplayedAt = -Infinity;
  let message = supported ? "Notifications are off for this tab."
    : "System notifications are unavailable here. Keep the Detections view open, or use a supported browser on localhost or HTTPS.";

  const validCursor = (value) => value && typeof value.epoch === "string"
    && /^[0-9a-f]{32}$/.test(value.epoch) && typeof value.id === "string"
    && /^(0|[1-9][0-9]{0,18})$/.test(value.id) && BigInt(value.id) <= 9_223_372_036_854_775_807n;

  function storedCursors() {
    if (!persistentScope || !storage) return [];
    try {
      const raw = storage.getItem(NOTIFICATION_CURSOR_KEY);
      if (!raw || raw.length > 24_576) return [];
      const rows = JSON.parse(raw);
      return Array.isArray(rows) && rows.length <= 8
        ? rows.filter((row) => validCursor(row) && typeof row.scope === "string" && row.scope.length <= 2048) : [];
    } catch (_) { return []; }
  }
  function remember(cursor) {
    if (!validCursor(cursor)) return false;
    if (highWater?.epoch !== cursor.epoch) {
      const saved = storedCursors().find((row) => row.scope === persistentScope && row.epoch === cursor.epoch);
      highWater = { epoch: cursor.epoch, id: saved?.id ?? "0" };
    }
    if (BigInt(cursor.id) <= BigInt(highWater.id)) return false;
    highWater = { epoch: cursor.epoch, id: cursor.id };
    return true;
  }
  function persist() {
    if (!persistentScope || !storage || !highWater) return;
    try {
      const rows = storedCursors().filter((row) => row.scope !== persistentScope);
      rows.push({ scope: persistentScope, ...highWater });
      storage.setItem(NOTIFICATION_CURSOR_KEY, JSON.stringify(rows.slice(-8)));
    } catch (_) { /* Private browsing may offer memory-only deduplication. */ }
  }
  function publishState() {
    try { onChange({ enabled, requesting, supported, message }); } catch (_) { /* UI is optional. */ }
  }
  function closeCurrent() {
    const previous = current;
    current = null;
    if (!previous) return;
    previous.onclick = previous.onclose = previous.onerror = null;
    try { previous.close(); } catch (_) { /* Revocation may already have closed it. */ }
  }
  function disable(reason = "Notifications are off for this tab.") {
    if (enabled) persist();
    enabled = requesting = false;
    generation += 1;
    pending = null;
    if (timer !== null) clearTimer(timer);
    timer = null;
    closeCurrent();
    message = reason;
    publishState();
  }
  function checkPermission() {
    if (!enabled || disposed) return false;
    if (NotificationImpl.permission === "granted") return true;
    disable("Notification permission was removed. Allow notifications in browser site settings, then enable them again.");
    return false;
  }
  function displayPending() {
    if (!pending || !checkPermission() || now() - lastDisplayedAt < NOTIFICATION_INTERVAL_MS) return;
    const target = pending;
    pending = null;
    const expected = generation;
    persist();
    closeCurrent();
    try {
      const notification = new NotificationImpl("FrankenTerm needs attention", {
        body: "A new warning, error, or input request was detected. Open the viewer to review it.",
        tag: "frankenterm-detection-attention",
        silent: true,
      });
      current = notification;
      lastDisplayedAt = now();
      notification.onclick = () => {
        if (generation !== expected || current !== notification || !checkPermission()) return;
        closeCurrent();
        Promise.resolve().then(() => {
          if (generation === expected && checkPermission()) return onInspect(target);
        }).catch(() => {
          if (generation !== expected || disposed) return;
          message = "The detection could not be opened. Return to the Detections view to inspect it.";
          publishState();
        });
      };
      notification.onclose = () => { if (current === notification) current = null; };
      notification.onerror = () => {
        if (generation === expected && current === notification) {
          disable("This browser could not display notifications. Use the Detections view; mobile browsers may require an installed app.");
        }
      };
    } catch (_) {
      disable("This browser could not display notifications. Use the Detections view; mobile browsers may require an installed app.");
    }
  }
  function schedule() {
    const expected = generation;
    timer = setTimer(() => {
      if (expected !== generation || !checkPermission()) return;
      timer = null;
      displayPending();
      if (enabled && generation === expected) schedule();
    }, NOTIFICATION_INTERVAL_MS);
  }
  publishState();
  return {
    async enable() {
      if (!supported || enabled || requesting || disposed) return;
      const expected = ++generation;
      requesting = true;
      message = "Allow notifications in the browser prompt to enable this tab.";
      publishState();
      let permission = NotificationImpl.permission;
      try {
        if (permission === "default") permission = await NotificationImpl.requestPermission();
      } catch (_) { permission = "unavailable"; }
      if (disposed || generation !== expected) return;
      requesting = false;
      if (permission !== "granted" || NotificationImpl.permission !== "granted") {
        disable(permission === "denied"
          ? "Notifications are blocked. Allow them in browser site settings, then enable them again."
          : "Notifications were not enabled. Use the Detections view or try a supported browser on localhost or HTTPS.");
        return;
      }
      enabled = true;
      message = "Notifications are on for this tab. Only detection and session checks continue when it is hidden.";
      persist();
      publishState();
      if (enabled && generation === expected) schedule();
    },
    disable,
    isEnabled: () => enabled && !disposed,
    isRequesting: () => requesting,
    checkpoint(cursor) { if (!disposed) remember(cursor); },
    detection(target) {
      // Advance even while disabled: opting in must not announce old frames.
      if (disposed || !remember(target) || !checkPermission()) return;
      if (!needsAttention(target)) return;
      // A burst keeps one latest target, one notification and one timer.
      pending = { epoch: target.epoch, id: target.id, paneId: target.paneId, paneUuid: target.paneUuid ?? null };
      displayPending();
    },
    report(value) { if (!disposed) { message = value; publishState(); } },
    dispose() { if (!disposed) { disposed = true; disable(); } },
  };
}

// The inventory contains retained panes, not a liveness promise. Never resolve
// a notification through today's selected row, a rounded ID, or a reused ID.
export function notificationPaneExists(target, payload) {
  if (typeof target?.paneUuid !== "string" || !target.paneUuid.length) return false;
  try {
    const { frame, exact } = decodeFrame(payload);
    if (frame?.ok !== true || !Array.isArray(frame.data?.panes)) return false;
    const matches = frame.data.panes.filter((pane, index) =>
      typeof pane?.pane_id === "number" && exact.data.panes[index]?.pane_id === target.paneId);
    return matches.length === 1 && matches[0].pane_uuid === target.paneUuid;
  } catch (_) { return false; }
}

// This feed subscribes to all panes. Retain the complete server token, and
// compare IDs only within its epoch and scope. Number would round i64 IDs.
function eventCursor(token) {
  if (typeof token !== "string" || token.length > 128) return null;
  const match = /^event-v1:([0-9a-f]{32}):all:(0|[1-9][0-9]{0,18})$/.exec(token);
  if (!match) return null;
  const id = BigInt(match[2]);
  return id <= 9_223_372_036_854_775_807n
    ? { token, epoch: match[1], id, decimal: match[2] } : null;
}

function streamFrame(payload, kind) {
  const limit = kind === "event" ? STREAM_FRAME_MAX_CHARS : 4096;
  if (typeof payload !== "string" || payload.length > limit) throw new Error("invalid frame size");
  const decoded = decodeFrame(payload);
  const frame = decoded.frame;
  if (frame?.schema !== "ft.stream.v1" || frame.stream !== "events" || frame.kind !== kind
      || !frame.data || typeof frame.data !== "object" || Array.isArray(frame.data)) {
    throw new Error("invalid frame envelope");
  }
  return decoded;
}

function streamErrorMessage(payload) {
  // Do not echo arbitrary server/proxy text or allocate a second huge object.
  if (payload.length > 4096) return "stream: invalid server error frame";
  try {
    const frame = JSON.parse(payload);
    const code = frame?.data?.code;
    if (frame.schema === "ft.stream.v1" && frame.stream === "events" && frame.kind === "error"
        && typeof code === "string" && /^[a-zA-Z0-9_.-]{1,80}$/.test(code)) {
      return `stream: ${code}`;
    }
  } catch (_) { /* finite diagnostic below */ }
  return "stream: invalid server error frame";
}

// One owner for the EventSource and its replacement timer. Native transient
// reconnects retain their own Last-Event-ID; a replacement source carries the
// last accepted ready/event/page checkpoint in ?cursor. EventSource inherits that
// ID on live-only and control messages, which cannot advance this boundary.
// Retire a rejected source before its private cursor can skip the rejected
// frame on reconnect. A discontinuity requires an explicit new live view.
export function createFleetStream(viewer, {
  apiRoot,
  EventSourceImpl = globalThis.EventSource,
  setTimer = globalThis.setTimeout,
  clearTimer = globalThis.clearTimeout,
  onResync = () => {},
  onDetection = () => {},
  onCheckpoint = () => {},
  onHalt = () => {},
  readyTimeoutMs = 10_000,
} = {}) {
  if (!Number.isSafeInteger(readyTimeoutMs) || readyTimeoutMs < 1 || readyTimeoutMs > 60_000) {
    throw new Error("invalid stream readiness deadline");
  }
  let source = null;
  let retryTimer = null;
  let readyTimer = null;
  let readyGeneration = 0;
  let retryMs = 1_000;
  let cursor = null;
  let terminalError = null;
  let stopped = true;
  let generation = 0;

  function clearReadyDeadline() {
    readyGeneration += 1;
    if (readyTimer !== null) clearTimer(readyTimer);
    readyTimer = null;
  }

  function awaitReady(current) {
    // Native EventSource retries are allowed a finite grace period. Repeated
    // errors or TCP opens cannot renew it without an admitted ready frame.
    // SSE keepalives are comments, so this is never an idle-stream timeout.
    if (readyTimer !== null) return;
    const expected = ++readyGeneration;
    readyTimer = setTimer(() => {
      if (!current() || readyGeneration !== expected) return;
      readyTimer = null;
      reconnect("stream: ready checkpoint timed out; reconnecting");
    }, readyTimeoutMs);
  }

  function resync() {
    const expectedGeneration = generation;
    try {
      Promise.resolve(onResync()).catch(() => {
        if (!stopped && generation === expectedGeneration) {
          viewer.reportError("stream: snapshot recovery failed");
        }
      });
    } catch (_) {
      viewer.reportError("stream: snapshot recovery failed");
    }
  }

  function retire() {
    generation += 1;
    clearReadyDeadline();
    if (retryTimer !== null) clearTimer(retryTimer);
    retryTimer = null;
    const previous = source;
    source = null;
    if (previous) previous.close();
  }

  function halt(message) {
    terminalError = `${message}; live updates stopped. Reload to start a new live view.`;
    retire();
    viewer.setStreamState(STREAM.DISCONNECTED);
    viewer.reportError(terminalError);
    try { onHalt(); } catch (_) { /* Feed retirement is already complete. */ }
  }

  function reconnect(message) {
    retire();
    if (stopped || terminalError !== null) return;
    viewer.setStreamState(STREAM.DISCONNECTED);
    if (message) viewer.reportError(message);
    const expectedGeneration = generation;
    retryTimer = setTimer(() => {
      if (stopped || generation !== expectedGeneration) return;
      retryTimer = null;
      open();
    }, retryMs);
    retryMs = Math.min(STREAM_RETRY_MAX_MS, retryMs * 2);
  }

  function rejectFrame(message) {
    if (cursor === null) halt(`${message}; no accepted checkpoint`);
    else reconnect(message);
  }

  function inheritedCursorIsSafe(token) {
    const inherited = eventCursor(token);
    return cursor !== null && inherited !== null
      && inherited.epoch === cursor.epoch && inherited.id <= cursor.id;
  }

  function open() {
    if (stopped || terminalError !== null || source !== null) return;
    viewer.setStreamState(STREAM.CONNECTING);
    const url = new URL("stream/events?channel=all&max_hz=20", apiRoot);
    if (cursor !== null) url.searchParams.set("cursor", cursor.token);
    let candidate;
    try {
      candidate = new EventSourceImpl(url);
    } catch (_) {
      reconnect("stream: connection could not be opened");
      return;
    }
    source = candidate;
    let ready = false;
    const current = () => !stopped && source === candidate;
    awaitReady(current);
    const onFrame = (message) => {
      if (!current()) return;
      let frame;
      let exact;
      try {
        ({ frame, exact } = streamFrame(message.data, message.type));
      } catch (_) {
        rejectFrame("stream: invalid stream frame");
        return;
      }
      let nextCursor = null;
      if (message.type === "ready") {
        nextCursor = eventCursor(frame.data.cursor);
        if (nextCursor === null || message.lastEventId !== nextCursor.token
            || frame.data.channel !== "all" || frame.data.pane_id !== null
            || frame.data.durable_detections !== true) {
          rejectFrame("stream: invalid ready checkpoint");
          return;
        }
        if (cursor !== null && nextCursor.epoch !== cursor.epoch) {
          halt("stream: event history changed");
          return;
        }
        if (cursor !== null && nextCursor.id > cursor.id) {
          rejectFrame("stream: ready checkpoint skipped the accepted cursor");
          return;
        }
      } else if (!ready) {
        rejectFrame("stream: frame arrived before a ready checkpoint");
        return;
      } else if (message.type === "checkpoint") {
        nextCursor = eventCursor(frame.data.cursor);
        if (nextCursor === null || message.lastEventId !== nextCursor.token
            || frame.data.pane_id !== null || frame.data.reason !== "examined_through_page"
            || frame.data.event !== undefined) {
          rejectFrame("stream: invalid page checkpoint");
          return;
        }
        if (nextCursor.epoch !== cursor.epoch) {
          halt("stream: event history changed");
          return;
        }
        if (nextCursor.id <= cursor.id) return;
      } else if (message.type === "event" && frame.data.delivery === "durable") {
        nextCursor = eventCursor(message.lastEventId);
        const event = frame.data.event;
        if (nextCursor === null || nextCursor.id === 0n || event?.type !== "pattern_detected"
            || typeof event.event_id !== "number" || exact.data.event?.event_id !== nextCursor.decimal) {
          rejectFrame("stream: invalid persisted event cursor");
          return;
        }
        if (nextCursor.epoch !== cursor.epoch) {
          halt("stream: event history changed");
          return;
        }
        if (nextCursor.id <= cursor.id) return; // same history, already admitted
      } else {
        // No `id:` is sent on live-only or lag frames. The browser still
        // exposes its inherited Last-Event-ID; it is never a new checkpoint.
        if (!inheritedCursorIsSafe(message.lastEventId)
            || (message.type === "event" && (frame.data.delivery !== "live_only"
                || !frame.data.event || typeof frame.data.event !== "object"
                || Array.isArray(frame.data.event) || frame.data.event.event_id != null))
            || (message.type === "lag" && frame.data.live_only !== true)) {
          rejectFrame("stream: invalid live-only frame");
          return;
        }
      }
      try {
        viewer.ingestStreamFrame(message.data);
      } catch (_) {
        rejectFrame("stream: frame rejected; retrying from the last accepted cursor");
        return;
      }
      if (message.type === "ready") {
        if (cursor === null) cursor = nextCursor;
        ready = true;
        clearReadyDeadline();
      } else if (nextCursor !== null) {
        cursor = nextCursor;
        // A TCP open/ready followed by a replay failure is not recovery.
        retryMs = 1_000;
      }
      // Notify only after the model admits a durable event and its exact
      // cursor. Snapshot replay, live-only frames and rejected rows never alert.
      try {
        if (nextCursor && message.type === "event") {
          const event = frame.data.event;
          const paneId = exact.data.event.pane_id;
          if (typeof event.pane_id === "number" && typeof paneId === "string"
              && /^(0|[1-9][0-9]{0,19})$/.test(paneId)
              && BigInt(paneId) <= 18_446_744_073_709_551_615n) {
            onDetection({
              epoch: nextCursor.epoch, id: nextCursor.decimal, paneId,
              paneUuid: typeof event.pane_uuid === "string" && event.pane_uuid.length <= 128 ? event.pane_uuid : null,
              severity: ["info", "warning", "error", "critical"].includes(event.detection?.severity)
                ? event.detection.severity : null,
              eventType: typeof event.detection?.event_type === "string" && event.detection.event_type.length <= 128
                ? event.detection.event_type : null,
            }, Number.isSafeInteger(frame.data.detected_at) && frame.data.detected_at >= 0 ? frame.data.detected_at : null,
            Number.isSafeInteger(frame.ts_ms) && frame.ts_ms >= 0 ? frame.ts_ms : null);
          }
        } else if (nextCursor) onCheckpoint({ epoch: nextCursor.epoch, id: nextCursor.decimal });
      } catch (_) { /* Optional system notifications cannot reject an admitted frame. */ }
      if (message.type === "ready" || message.type === "lag") resync();
    };
    for (const type of ["ready", "event", "checkpoint", "lag"]) candidate.addEventListener(type, onFrame);
    candidate.addEventListener("cursor_discontinuity", (message) => {
      if (!current()) return;
      let reason = "invalid cursor discontinuity";
      try {
        const { frame } = streamFrame(message.data, "cursor_discontinuity");
        const resume = eventCursor(frame.data.resume_cursor);
        if (frame.data.code === "event_cursor_discontinuity" && frame.data.terminal === true
            && ["retention_pruned", "legacy_history_unavailable", "cursor_epoch_mismatch",
              "cursor_ahead_of_high_water"].includes(frame.data.reason)
            && resume !== null && (cursor === null
              || (resume.epoch === cursor.epoch && resume.id <= cursor.id))) {
          reason = `event_cursor_discontinuity (${frame.data.reason})`;
        }
      } catch (_) { /* terminal finite diagnostic below */ }
      halt(`stream: ${reason}`);
    });
    candidate.addEventListener("error", (message) => {
      if (!current()) return;
      // Named SSE `event: error` is a MessageEvent, whereas a network error
      // is an Event without data. Do not discard replay/storage failures as
      // if they were merely an EventSource transport state transition.
      if (typeof message.data === "string") {
        reconnect(streamErrorMessage(message.data));
      } else if (candidate.readyState === EventSourceImpl.CLOSED) {
        reconnect("stream: disconnected; reconnecting");
      } else {
        ready = false;
        viewer.setStreamState(STREAM.CONNECTING);
        awaitReady(current);
      }
    });
  }

  return {
    start() {
      if (!stopped) return;
      stopped = false;
      generation += 1;
      if (terminalError !== null) {
        viewer.setStreamState(STREAM.DISCONNECTED);
        viewer.reportError(terminalError);
      } else open();
    },
    stop() {
      stopped = true;
      retire();
      viewer.setStreamState(STREAM.DISCONNECTED);
    },
  };
}

// Read incrementally: checking a Rust decoder's limit after response.text()
// is too late to bound a hostile or accidentally unbounded HTTP body.
async function readSnapshot(path, apiRoot, fetchImpl, signal, bodyLimitBytes) {
  let reader;
  let abortListener;
  const aborted = new Promise((_, reject) => {
    abortListener = () => {
      if (reader) void reader.cancel().catch(() => {});
      reject(new Error("request cancelled or timed out"));
    };
    if (signal.aborted) abortListener();
    else signal.addEventListener("abort", abortListener, { once: true });
  });
  const work = (async () => {
    if (signal.aborted) throw new Error("request cancelled or timed out");
    let response;
    try {
      response = await fetchImpl(new URL(path, apiRoot), {
        cache: "no-store", credentials: "same-origin", signal,
      });
    } catch (_) {
      throw new Error("network request failed");
    }
    try {
      if (signal.aborted) throw new Error("request cancelled or timed out");
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const declared = response.headers.get("content-length");
      if (declared && /^[0-9]+$/.test(declared) && Number(declared) > bodyLimitBytes) {
        throw new Error("response exceeds byte limit");
      }
      if (!response.body) throw new Error("response has no readable body");
      reader = response.body.getReader();
      const decoder = new TextDecoder("utf-8", { fatal: true });
      let bytes = 0;
      let text = "";
      for (;;) {
        const { done, value } = await reader.read();
        if (signal.aborted) throw new Error("request cancelled or timed out");
        if (done) return text + decoder.decode();
        if (value.byteLength > bodyLimitBytes - bytes) {
          throw new Error("response exceeds byte limit");
        }
        bytes += value.byteLength;
        text += decoder.decode(value, { stream: true });
      }
    } finally {
      // Cancellation is best effort, not another unbounded wait. A late
      // response after stop is disposed here and never reaches the model.
      if (reader) {
        void reader.cancel().catch(() => {});
        reader.releaseLock();
      } else if (response.body) {
        void response.body.cancel().catch(() => {});
      }
    }
  })();
  try {
    return await Promise.race([work, aborted]);
  } finally {
    signal.removeEventListener("abort", abortListener);
  }
}

// Snapshot and stream responses have no shared network ordering. Keep only
// accepted, surface-specific live frames that arrived during a refresh, then
// reapply them after that surface's replacement snapshot. Failed or overflowed
// snapshots leave the live model intact. Lag/gap counters are never replayed.
export function createFleetRefresh(viewer, {
  apiRoot,
  fetchImpl = globalThis.fetch,
  setTimer = globalThis.setTimeout,
  clearTimer = globalThis.clearTimeout,
  timeoutMs = 10_000,
  bodyLimitBytes = 8 * 1024 * 1024,
  journalLimitBytes = 2 * 1024 * 1024,
  journalLimitFrames = 512,
} = {}) {
  for (const limit of [timeoutMs, bodyLimitBytes, journalLimitBytes, journalLimitFrames]) {
    if (!Number.isSafeInteger(limit) || limit <= 0) throw new Error("invalid snapshot limit");
  }
  let stopped = true;
  let flight = null;
  let streamError = null;
  const newJournal = () => ({ frames: [], bytes: 0, overflow: false });

  async function run(batch) {
    const deadline = setTimer(() => batch.controller.abort(), timeoutMs);
    try {
      const results = await Promise.allSettled([
        readSnapshot("panes", apiRoot, fetchImpl, batch.controller.signal, bodyLimitBytes),
        readSnapshot("events?limit=200", apiRoot, fetchImpl, batch.controller.signal, bodyLimitBytes),
      ]);
      if (stopped || flight !== batch) return false;
      const failures = [];
      for (const [index, surface] of ["panes", "events"].entries()) {
        const result = results[index];
        const journal = batch[surface];
        if (result.status === "rejected") {
          // Only our own finite diagnostics escape; decoder/transport errors
          // can otherwise contain arbitrary response text or proxy details.
          const reason = result.reason?.message;
          const safe = typeof reason === "string" &&
            /^(HTTP [0-9]{3}|network request failed|request cancelled or timed out|response exceeds byte limit|response has no readable body)$/.test(reason);
          failures.push(`${surface}: ${safe ? reason : "response could not be read"}`);
        } else if (journal.overflow) {
          failures.push(`${surface}: snapshot superseded by live traffic`);
        } else {
          let replaced = false;
          try {
            if (surface === "panes") viewer.ingestPanes(result.value);
            else viewer.ingestEvents(result.value);
            replaced = true;
          } catch (_) {
            failures.push(`${surface}: snapshot rejected`);
          }
          if (replaced) {
            try {
              for (const payload of journal.frames) viewer.ingestStreamFrame(payload);
            } catch (_) {
              failures.push(`${surface}: live reconciliation failed`);
            }
          }
        }
      }
      // A successful second endpoint must not hide the first one's failure,
      // nor may a background refresh erase a still-unrecovered stream error.
      if (streamError) failures.push(streamError);
      if (failures.length) viewer.reportError(failures.join("; "));
      return failures.length === 0;
    } finally {
      clearTimer(deadline);
    }
  }

  function refresh() {
    if (stopped) return Promise.resolve(false);
    if (flight) {
      flight.again = true;
      return flight.promise;
    }
    const batch = {
      controller: new AbortController(), panes: newJournal(), events: newJournal(),
      again: false, promise: null,
    };
    flight = batch;
    batch.promise = run(batch).finally(() => {
      if (flight !== batch) return;
      flight = null;
      if (!stopped && batch.again) void refresh();
    });
    return batch.promise;
  }

  return {
    refresh,
    start() { stopped = false; },
    stop() {
      stopped = true;
      const previous = flight;
      flight = null;
      if (previous) previous.controller.abort();
    },
    // This facade is passed to createFleetStream. The model admits a frame
    // before we journal it or allow the stream owner to acknowledge its ID.
    ingestStreamFrame(payload) {
      if (stopped) throw new Error("snapshot owner stopped");
      viewer.ingestStreamFrame(payload);
      const frame = JSON.parse(payload);
      if (["ready", "event", "checkpoint"].includes(frame.kind)) streamError = null;
      if (!flight || frame.kind !== "event") return;
      const type = frame.data?.event?.type;
      const surface = type === "pattern_detected" ? "events"
        : ["pane_discovered", "pane_disappeared"].includes(type) ? "panes" : null;
      if (!surface) return;
      const journal = flight[surface];
      if (journal.overflow) return;
      // JS strings use UTF-16 storage. Charge two bytes per code unit, not
      // UTF-8 wire length, so the retained journal has a conservative bound.
      const bytes = payload.length * 2;
      if (journal.frames.length >= journalLimitFrames || bytes > journalLimitBytes - journal.bytes) {
        journal.overflow = true;
        journal.frames = [];
        journal.bytes = 0;
      } else {
        journal.frames.push(payload);
        journal.bytes += bytes;
      }
    },
    reportError(message) {
      if (stopped) return;
      streamError = String(message).slice(0, 200);
      viewer.reportError(streamError);
    },
    setStreamState(state) { viewer.setStreamState(state); },
  };
}

function keyInput(event, phase) {
  return {
    kind: "key",
    phase,
    key: typeof event.key === "string" ? event.key : "",
    code: typeof event.code === "string" ? event.code : "",
    mods:
      (event.shiftKey ? 1 : 0) |
      (event.altKey ? 2 : 0) |
      (event.ctrlKey ? 4 : 0) |
      (event.metaKey ? 8 : 0),
    repeat: Boolean(event.repeat),
  };
}

// Native controls own their keys. In particular Tab must escape the canvas,
// and typing in the pane selector/search box must not move the fleet selection.
export function handleFleetKey(event, { canvas, term, viewer, inspector }) {
  if (event.target !== canvas || event.metaKey || event.ctrlKey || event.altKey
      || event.key === "Tab" || event.isComposing) return;
  // The native text view keeps Shift navigation for browser text selection.
  if (!term && event.shiftKey && event.key !== "G") return;
  if (event.key === "Enter") {
    event.preventDefault();
    if (!event.repeat) inspector.openSelected();
    return;
  }
  if (!term) {
    // This is the pinned ftui-web input_parser contract, also emitted by the
    // renderer. Both presentations drive the very same Rust fleet model.
    if (TEXT_NAV_KEYS.has(event.key) && viewer.pushEncodedInput(JSON.stringify(keyInput(event, "down")))) {
      event.preventDefault();
    }
    return;
  }
  try {
    term.input(keyInput(event, "down"));
  } finally {
    for (const json of term.drainEncodedInputs()) viewer.pushEncodedInput(json);
    term.drainEncodedInputBytes();
    term.drainImeCompositionJsonl("fleet-viewer", 0n, "");
  }
  if (FORWARDED_NAV_KEYS.has(event.key)) event.preventDefault();
}

// Mount the ordinary DOM tools before importing either WASM bundle. A missing
// model still permits manual pane/history inspection; a missing renderer uses
// the same FleetViewer, snapshots and durable stream through a native text view.
export function mountFleetApp({
  document: doc = globalThis.document,
  window: host = globalThis.window,
  apiRoot = new URL("..", host.location.href),
  loadViewer = () => import("./pkg/ft_fleet_viewer.js"),
  loadRenderer = () => import("./pkg/FrankenTerm.js"),
  fetchImpl = globalThis.fetch,
  EventSourceImpl = globalThis.EventSource,
  NotificationImpl = host.Notification,
  notificationStorage,
  notificationScope = null,
  onNotificationsChanged = () => {},
  verifyNotificationSession = async () => true,
  setTimer = globalThis.setTimeout,
  clearTimer = globalThis.clearTimeout,
  setRepeating = globalThis.setInterval,
  clearRepeating = globalThis.clearInterval,
  requestFrame = (callback) => host.requestAnimationFrame(callback),
  cancelFrame = (handle) => host.cancelAnimationFrame(handle),
  ResizeObserverImpl = globalThis.ResizeObserver,
  now = Date.now,
  managePageLifecycle = true,
} = {}) {
  const element = (id) => doc.getElementById(id);
  const viewport = element("fleet-viewport");
  const canvas = element("terminal");
  const screenText = element("screen-text");
  const modeLine = element("fleet-mode");
  const selectionLine = element("fleet-selection");
  const textToggle = element("fleet-text-toggle");
  const notificationToggle = element("fleet-notifications-toggle");
  const notificationStatus = element("fleet-notifications-status");
  const navigation = new Map([
    ["fleet-panes", "1"], ["fleet-events", "2"], ["fleet-first", "Home"],
    ["fleet-previous", "ArrowUp"], ["fleet-next", "ArrowDown"],
    ["fleet-last", "End"], ["fleet-refresh", "r"],
  ]);
  let viewer = null;
  let term = null;
  let snapshots = null;
  let stream = null;
  let modelFailed = false;
  let rendererFailed = false;
  let preferText = false;
  let active = false;
  let pagePresent = true;
  let activityGeneration = 0;
  let disposed = false;
  let networkActive = false;
  let backgroundNotifications = false;
  let notificationsHalted = false;
  let notifications = null;
  let inspection = null;
  let inspectionFromQueue = false;
  let inspectionController = null;
  let inspectionGeneration = 0;
  let refreshTimer = null;
  let frameHandle = null;
  let lastClock = 0;
  const textView = () => !term || preferText;
  const setText = (node, text) => { if (node.textContent !== text) node.textContent = text; };
  const textSelected = () => {
    const selection = host.getSelection?.();
    return !selection?.isCollapsed && selection?.rangeCount > 0
      && (screenText.contains(selection.anchorNode) || screenText.contains(selection.focusNode));
  };
  const clearTextSelection = () => { if (textSelected()) host.getSelection().removeAllRanges(); };

  function updateMode() {
    const asText = textView();
    const moveFocus = doc.activeElement === (asText ? canvas : screenText);
    canvas.hidden = asText;
    canvas.tabIndex = asText ? -1 : 0;
    screenText.className = asText ? "fleet-text" : "visually-hidden";
    screenText.tabIndex = asText && viewer ? 0 : -1;
    element("fleet-text-help").hidden = !asText || !viewer;
    for (const id of navigation.keys()) element(id).disabled = !viewer;
    element("pane-output-open").disabled = !viewer;
    textToggle.disabled = !viewer || !term;
    notificationToggle.disabled = !viewer || disposed || notificationsHalted || typeof NotificationImpl !== "function"
      || typeof NotificationImpl.requestPermission !== "function" || notifications?.isRequesting();
    textToggle.setAttribute("aria-pressed", String(asText));
    setText(textToggle, asText ? "Use canvas view" : "Use text view");
    const mode = viewer ? (asText ? "text" : "canvas") : modelFailed ? "capture-only" : "loading";
    doc.documentElement.dataset.viewerMode = mode;
    doc.documentElement.dataset.viewerReady = !active ? "suspended"
      : viewer ? "1" : modelFailed ? "capture-only" : "loading";
    const message = modelFailed
      ? "Fleet inventory is unavailable. Search captured history or open a pane by ID above; both tools remain available."
      : !viewer ? "Loading fleet inventory. Search captured history or open a pane by ID above."
      : !asText ? "Live fleet inventory. Text view and captured output are read-only."
      : textSelected() ? "Text is held steady while selected. Clear the selection to show the latest fleet frame."
      : rendererFailed ? "Canvas rendering is unavailable. Live fleet text, navigation, captured output and history search remain available."
      : term ? "Live fleet text. Navigate with the buttons or keyboard; select text to copy."
      : "Live fleet text is ready while canvas rendering starts. Navigation and captured-history tools are available.";
    setText(modeLine, message);
    if (moveFocus && active && viewer) (asText ? screenText : canvas).focus();
  }

  function mirrorText() {
    if (!viewer) return;
    // Native selection must survive clock ticks and incoming frames. Keep
    // draining model patches while copying; resume from its latest frame.
    if (!textView() || !textSelected()) {
      const text = viewer.screenText();
      if (typeof text !== "string" || text.length > SCREEN_TEXT_MAX_CHARS) throw new Error("invalid fleet frame");
      setText(screenText, text);
    }
    const paneId = viewer.selectedPaneId();
    setText(selectionLine, typeof paneId === "string" ? `Selected pane ${paneId}. Enter or Inspect selected pane opens its captured output.`
      : "No pane or detection selected. Open captured output by pane ID, or search history.");
  }

  function retireRenderer() {
    const previous = term;
    term = null;
    rendererFailed = true;
    try { previous?.destroy(); } catch (_) { /* a lost renderer must not stop DOM tools */ }
    if (!disposed) updateMode();
  }

  function present() {
    // takeFlatPatches also drains StepProgram's output buffers in text mode.
    const { cells, spans } = viewer.takeFlatPatches();
    if (!active) return; // Drain bounded model output without rendering a hidden page.
    if (term && !preferText) {
      try {
        if (cells.length > 0) term.applyPatchBatchFlat(spans, cells);
        term.render();
      } catch (_) { retireRenderer(); }
    }
    mirrorText();
  }

  function stopFleet({ keepNotifications = false } = {}) {
    backgroundNotifications = keepNotifications && notifications?.isEnabled() && !disposed && pagePresent;
    if (refreshTimer !== null) clearRepeating(refreshTimer);
    refreshTimer = null;
    try {
      if (!backgroundNotifications) { networkActive = false; stream?.stop(); }
    } finally {
      snapshots?.stop(); // Always abort pending inventory/history refreshes.
      if (backgroundNotifications) snapshots?.start(); // Admit only the bounded live feed.
    }
  }

  function failModel() {
    if (disposed) return;
    attention.clear();
    notifications?.disable("Fleet detections are unavailable. Reload the viewer before enabling notifications.");
    try { stopFleet(); } catch (_) { /* stop still aborts outstanding snapshots */ }
    viewer = null;
    modelFailed = true;
    retireRenderer();
    setText(screenText, "Captured history and manual pane output remain available in the toolbar above.");
    setText(selectionLine, "Enter a pane ID above, or search captured history.");
  }

  function advance() {
    if (!viewer) return;
    try { if (viewer.step()) present(); } catch (_) { failModel(); }
  }

  const refresh = () => {
    if (active && snapshots && viewer) void snapshots.refresh().catch(failModel);
  };
  function startFleet() {
    if (!active || !viewer || !snapshots || refreshTimer !== null) return;
    backgroundNotifications = false;
    snapshots.start();
    if (!networkActive) { networkActive = true; stream.start(); }
    refresh();
    const expected = activityGeneration;
    refreshTimer = setRepeating(() => {
      if (expected === activityGeneration) refresh();
    }, REFRESH_MS);
  }

  const pinnedOutputs = mountPinnedOutputs({
    document: doc, apiRoot,
    createOutput: () => createPaneOutput({
      apiRoot, EventSourceImpl, setTimer, clearTimer, maxChars: 128 * 1024, maxChunks: 1024,
    }),
  });
  const inspector = mountPaneOutput({
    document: doc, apiRoot,
    output: createPaneOutput({ apiRoot, EventSourceImpl, setTimer, clearTimer }),
    onPin: pinnedOutputs.pin,
    onOpen() { cancelInspection(); pinnedOutputs.showAll(); },
    restoreFocus: () => {
      if (active) (viewer ? (textView() ? screenText : canvas) : element("pane-output-id")).focus();
    },
    getSelectedPane: () => {
      // Apply queued navigation before sampling its identity. Once opened,
      // later row changes never retarget this pane's output subscription.
      advance();
      return { paneId: viewer?.selectedPaneId(), paneUuid: viewer?.selectedPaneUuid() };
    },
  });
  // A directly mounted, initially hidden page must not open manual output
  // while its model imports are pending. The auth owner also defers mounting.
  inspector.suspend();
  const historySearch = mountHistorySearch({
    document: doc,
    inspectCapture: (hit) => inspector.openCapture(hit),
    readBody: (path, signal) => active ? readSnapshot(path, apiRoot, fetchImpl, signal, 8 * 1024 * 1024)
      : Promise.reject(new Error("viewer suspended")),
    inspectPane: (paneId) => {
      // Route the exact captured identity through the same validated native
      // form as a manual Open action, never through the mutable fleet row.
      inspector.fillPaneForm(paneId);
      element("pane-output-form").requestSubmit();
    },
  });

  const attention = mountAttentionQueue(doc, (target, triggeringOutput = false) => {
    if (!active || disposed || !viewer) return;
    cancelInspection();
    inspection = { target, fromQueue: true, triggeringOutput };
    inspectionFromQueue = true;
    void inspectDetection();
  }, now);

  // A queue verification answers to the attention queue, not to
  // notifications, so a notification state change passes keepQueue.
  function cancelInspection({ keepQueue = false } = {}) {
    if (keepQueue && inspectionFromQueue) return;
    inspectionGeneration += 1;
    inspection = null;
    inspectionFromQueue = false;
    inspectionController?.abort();
    inspectionController = null;
  }
  async function inspectDetection() {
    if (!inspection || !active || disposed) return;
    const { target, fromQueue, triggeringOutput } = inspection;
    const admitted = () => fromQueue ? attention.contains(target) : notifications.isEnabled();
    if (!admitted()) return;
    // The result goes to the fleet view and the pane form, which pinned focus
    // mode hides. This runs only for an explicit click, so leave focus mode.
    pinnedOutputs.showAll();
    const report = (message) => fromQueue ? attention.report(message) : notifications.report(message);
    inspection = null;
    const expected = ++inspectionGeneration;
    const controller = new AbortController();
    inspectionController = controller;
    const current = () => !disposed && active && admitted()
      && inspectionGeneration === expected && !controller.signal.aborted;
    const deadline = setTimer(() => controller.abort(), 10_000);
    try {
      if (!await verifyNotificationSession()) {
        if (current()) report("Your session could not be verified yet. Review Detections or try again after the session check completes.");
        return;
      }
      if (!current()) return;
      if (triggeringOutput) {
        // Query the event's own source. A later registration can reuse the
        // pane ID, and a closed pane can still have a readable detection.
        const params = new URLSearchParams({
          cursor: `event-v1:${target.epoch}:all:${target.id}`,
          pane_id: target.paneId, pane_uuid: target.paneUuid,
        });
        const body = JSON.parse(await readSnapshot(`events/capture?${params}`, apiRoot, fetchImpl, controller.signal, 8192));
        if (!current()) return;
        const ref = body?.data;
        if (body?.ok !== true || ref?.event_epoch !== target.epoch || ref.event_id !== target.id
            || ref.pane_id !== target.paneId || ref.pane_uuid !== target.paneUuid
            || !inspector.openCapture({ paneId: ref.pane_id, paneUuid: ref.pane_uuid, segmentId: ref.segment_id,
              capturedAt: ref.captured_at, contentLength: ref.content_len, captureCursor: ref.capture_cursor })) {
          report("The triggering capture could not be verified.");
          return;
        }
        report("Opened the triggering capture.");
        return;
      }
      // Bring the read-only detection list forward only in response to this
      // explicit click. Never inject input into a terminal or attach control.
      viewer?.pushEncodedInput(JSON.stringify(keyInput({ key: "2" }, "down")));
      advance();
      const body = await readSnapshot("panes", apiRoot, fetchImpl, controller.signal, 8 * 1024 * 1024);
      if (!current()) return;
      if (/^[0-9a-f]{32}$/.test(target.paneUuid) && notificationPaneExists(target, body)) {
        inspector.fillPaneForm(target.paneId, target.paneUuid);
        report("The detected pane's stored identity was verified. Choose Open output to inspect retained capture; the pane may no longer be live.");
        element("pane-output-id").focus();
      } else {
        report("The detected pane is unavailable or its identity changed. Review Detections or search captured history.");
      }
    } catch (_) {
      if (!disposed && active && admitted() && inspectionGeneration === expected) {
        report(triggeringOutput ? "The triggering output is unavailable. It may have expired from retained history."
          : "The detected pane could not be verified. Review Detections or search captured history.");
      }
    } finally {
      clearTimer(deadline);
      if (inspectionController === controller) inspectionController = null;
    }
  }
  let cursorStorage = notificationStorage;
  if (cursorStorage === undefined) {
    try { cursorStorage = host.localStorage; } catch (_) { cursorStorage = null; }
  }
  notifications = createDetectionNotifications({
    NotificationImpl, storage: cursorStorage,
    scope: notificationScope ?? apiRoot.origin + apiRoot.pathname,
    setTimer, clearTimer, now,
    onChange(state) {
      notificationToggle.disabled = !viewer || disposed || notificationsHalted || !state.supported || state.requesting;
      notificationToggle.setAttribute("aria-pressed", String(state.enabled));
      setText(notificationToggle, state.enabled ? "Disable notifications" : "Enable notifications");
      setText(notificationStatus, state.message);
      if (!state.enabled) {
        cancelInspection({ keepQueue: true });
        if (backgroundNotifications) stopFleet();
      }
      onNotificationsChanged(state.enabled);
    },
    onInspect(target) {
      if (disposed || !notifications.isEnabled()) return;
      cancelInspection();
      inspection = { target, fromQueue: false };
      host.focus?.();
      if (active) return inspectDetection();
      notifications.report("Return to this tab to verify the session and inspect the detection.");
    },
  });
  notificationToggle.addEventListener("click", () => {
    if (!active || !viewer || disposed || notificationsHalted) return;
    if (notifications.isEnabled()) notifications.disable();
    else void notifications.enable();
  });

  host.addEventListener("keydown", (event) => {
    if (!active || !viewer) return;
    const target = textView() ? screenText : canvas;
    if (event.target === target && !event.metaKey && !event.ctrlKey && !event.altKey
        && !event.shiftKey && !event.isComposing && (TEXT_NAV_KEYS.has(event.key) || event.key === "Enter")) clearTextSelection();
    try { handleFleetKey(event, { canvas: target, term: textView() ? null : term, viewer, inspector }); }
    catch (_) { retireRenderer(); }
    advance();
  });
  for (const [id, key] of navigation) element(id).addEventListener("click", () => {
    if (!active || !viewer) return;
    clearTextSelection();
    try { viewer.pushEncodedInput(JSON.stringify(keyInput({ key }, "down"))); }
    catch (_) { failModel(); }
    advance();
    if (viewer?.takeRefreshRequest()) refresh();
  });
  textToggle.addEventListener("click", () => {
    if (!active || !viewer || !term) return;
    preferText = !preferText;
    clearTextSelection();
    updateMode();
    resize();
  });
  doc.addEventListener("selectionchange", () => {
    if (!active || !viewer || !textView()) return;
    try { mirrorText(); updateMode(); } catch (_) { failModel(); }
  });

  function resize() {
    if (!active || !viewer || viewport.clientWidth <= 0 || viewport.clientHeight <= 0) return;
    let cols = Math.max(80, Math.min(GRID_MAX_COLS, Math.floor(viewport.clientWidth / 8)));
    let rows = Math.max(12, Math.min(GRID_MAX_ROWS, Math.floor(viewport.clientHeight / 20)));
    if (term && !preferText) {
      try {
        const geometry = term.fitToContainer(Math.min(viewport.clientWidth, GRID_MAX_COLS * 8),
          Math.min(viewport.clientHeight, GRID_MAX_ROWS * 16), host.devicePixelRatio || 1);
        if (!Number.isInteger(geometry.cols) || !Number.isInteger(geometry.rows)
            || geometry.cols < 20 || geometry.cols > GRID_MAX_COLS || geometry.rows < 4 || geometry.rows > GRID_MAX_ROWS) {
          throw new Error("invalid renderer geometry");
        }
        ({ cols, rows } = geometry);
      } catch (_) { retireRenderer(); }
    }
    try {
      // The pinned StepProgram invalidates its diff baseline for every resize,
      // including unchanged dimensions: a late/restored canvas gets a full
      // frame without replacing the model, cursor, selection or network owner.
      if (!viewer.resize(cols, rows)) {
        advance();
        if (!viewer || !viewer.resize(cols, rows)) throw new Error("resize queue full");
      }
      advance();
    } catch (_) { failModel(); }
  }
  const resizeObserver = ResizeObserverImpl ? new ResizeObserverImpl(resize) : null;
  if (resizeObserver) resizeObserver.observe(viewport);
  else host.addEventListener("resize", resize);

  function scheduleFrame() {
    const expected = activityGeneration;
    frameHandle = requestFrame(() => {
      // Cancelling a frame does not revoke a callback already queued by the
      // browser. Do not let it replace a resumed generation's frame handle.
      if (!active || activityGeneration !== expected) return;
      frameHandle = null;
      frame();
    });
  }
  function frame() {
    if (!active) return;
    const time = now();
    if (viewer) {
      try {
        if (time - lastClock >= CLOCK_MS) { viewer.setTime(time); lastClock = time; }
        if (viewer.takeRefreshRequest()) refresh();
        advance();
      } catch (_) { failModel(); }
    }
    inspector.render();
    pinnedOutputs.render();
    attention.render();
    scheduleFrame();
  }
  function resume() {
    if (active || disposed || !pagePresent || doc.visibilityState === "hidden") return;
    active = true;
    activityGeneration += 1;
    inspector.restore();
    pinnedOutputs.restore();
    updateMode();
    resize();
    startFleet();
    scheduleFrame();
    if (inspection) void inspectDetection();
  }
  function suspend({ keepNotifications = false } = {}) {
    active = false;
    activityGeneration += 1;
    cancelInspection();
    if (notifications?.isRequesting()) notifications.disable("Enable notifications again after returning to this tab.");
    stopFleet({ keepNotifications });
    inspector.suspend();
    pinnedOutputs.suspend();
    historySearch.suspend();
    if (frameHandle !== null) cancelFrame(frameHandle);
    frameHandle = null;
    updateMode();
  }
  // Session retirement is terminal. A new login reloads the page so neither
  // captured data nor an old stream cursor crosses the authentication boundary.
  function dispose() {
    if (disposed) return;
    disposed = true;
    attention.clear();
    notifications.dispose();
    suspend();
    inspector.close();
    pinnedOutputs.dispose();
    historySearch.close();
    resizeObserver?.disconnect?.();
    clearTextSelection();
    retireRenderer();
    try { viewer?.free?.(); } catch (_) { /* the stopped model cannot publish */ }
    viewer = null;
    snapshots = null;
    stream = null;
    canvas.width = canvas.width; // clear the backing surface even after device loss
    screenText.textContent = "";
    selectionLine.textContent = "";
    element("pane-output-id").value = "";
    for (const id of [...navigation.keys(), "pane-output-open", "pane-output-submit", "pane-output-pin",
      "pane-output-id", "history-search-open", "fleet-text-toggle", "fleet-notifications-toggle"]) element(id).disabled = true;
    doc.documentElement.dataset.viewerReady = "locked";
  }
  if (managePageLifecycle) {
    host.addEventListener("pagehide", () => {
      pagePresent = false;
      notifications.disable();
      suspend();
    });
    host.addEventListener("pageshow", () => { pagePresent = true; resume(); });
    doc.addEventListener("visibilitychange", () => {
      if (doc.visibilityState === "hidden") suspend({ keepNotifications: notifications.isEnabled() });
      else resume();
    });
  }
  updateMode();
  resume();

  const modelReady = Promise.resolve().then(async () => {
    if (disposed) return false;
    const { default: initViewer, FleetViewer } = await loadViewer();
    if (disposed) return false;
    await initViewer();
    if (disposed) return false;
    viewer = new FleetViewer(80, 24);
    viewer.setTime(now());
    viewer.init();
    if (active) present();
    snapshots = createFleetRefresh(viewer, { apiRoot, fetchImpl, setTimer, clearTimer });
    stream = createFleetStream({
      ingestStreamFrame(payload) {
        snapshots.ingestStreamFrame(payload);
        if (!active) advance(); // No hidden RAF is needed to drain the bounded model.
      },
      setStreamState: snapshots.setStreamState,
      reportError: snapshots.reportError,
    }, {
      apiRoot, EventSourceImpl, setTimer, clearTimer, onResync: refresh,
      onDetection(target, detectedAt, sentAt) {
        attention.detection(target, detectedAt, sentAt);
        notifications.detection(target);
      },
      onCheckpoint: notifications.checkpoint,
      onHalt() {
        notificationsHalted = true;
        attention.clear();
        cancelInspection();
        notifications.disable("Detection history changed. Reload the viewer before enabling notifications again.");
      },
    });
    updateMode();
    resize();
    startFleet();
    return true;
  }).catch(() => { failModel(); return false; });

  const rendererReady = Promise.resolve().then(async () => {
    if (disposed) return;
    const { default: initRenderer, FrankenTermWeb } = await loadRenderer();
    if (disposed) return;
    await initRenderer();
    if (!await modelReady || !viewer) return;
    await doc.fonts?.load('16px "Pragmasevka NF"').catch(() => undefined);
    if (!viewer) return;
    const candidate = new FrankenTermWeb();
    try {
      const contract = typeof candidate.apiContract === "function" ? candidate.apiContract() : null;
      if (contract?.apiLine !== "frankenterm-js" || contract.apiVersion !== "1.0.0") throw new Error("unsupported renderer");
      await candidate.init(canvas, { cols: 80, rows: 24, cellWidth: 8, cellHeight: 16,
        dpr: host.devicePixelRatio || 1, focused: false });
      if (!viewer) { candidate.destroy(); return; }
      term = candidate;
      // A user already reading or selecting text must not have that surface
      // hidden when a slow GPU/font initialization finally completes.
      if (doc.activeElement === screenText || textSelected()) preferText = true;
      updateMode();
      resize();
    } catch (error) {
      try { candidate.destroy(); } catch (_) { /* best-effort failed initialization cleanup */ }
      throw error;
    }
  }).catch(() => { retireRenderer(); });
  return { ready: Promise.all([modelReady, rendererReady]), suspend, resume, dispose };
}

// Authenticate before mounting any data consumer. Session changes retire the
// entire viewer; sign-in reloads it instead of reusing a previous user's data,
// pending fetches or resumable stream cursor. Credentials never enter a URL or
// browser storage, and only the server can read the HttpOnly session cookie.
export function mountAuthenticatedFleet({
  document: doc = globalThis.document,
  window: host = globalThis.window,
  apiRoot = new URL("..", host.location.href),
  fetchImpl = globalThis.fetch,
  EventSourceImpl = globalThis.EventSource,
  mountApp = mountFleetApp,
  setTimer = globalThis.setTimeout,
  clearTimer = globalThis.clearTimeout,
  now = Date.now,
} = {}) {
  const element = (id) => doc.getElementById(id);
  const panel = element("web-auth");
  const form = element("web-auth-form");
  const token = element("web-auth-token");
  const submit = element("web-auth-submit");
  const status = element("web-auth-status");
  const retry = element("web-auth-retry");
  const logout = element("web-auth-logout");
  const controls = element("web-auth-controls");
  let app = null;
  let appSuspended = false;
  let backgroundNotifications = false;
  let mounted = false;
  let retired = false;
  let pagePresent = true;
  let active = doc.visibilityState !== "hidden";
  let disposed = false;
  let required = false;
  let sessionEpoch = null;
  let sessionVerified = false;
  let generation = 0;
  let activityGeneration = 0;
  let timerGeneration = 0;
  let pollTimer = null;
  let expiryTimer = null;
  let pending = null;
  let channel = null;
  try { if (host.BroadcastChannel) channel = new host.BroadcastChannel("frankenterm-web-session"); }
  catch (_) { /* server expiry and status polling remain authoritative */ }

  function clearTimers() {
    timerGeneration += 1;
    if (pollTimer !== null) clearTimer(pollTimer);
    if (expiryTimer !== null) clearTimer(expiryTimer);
    pollTimer = expiryTimer = null;
  }

  function showGate(message, state = "locked") {
    token.value = "";
    element("inspect-toolbar").hidden = true;
    element("workspace").hidden = true;
    controls.hidden = true;
    panel.hidden = false;
    form.hidden = !required;
    retry.hidden = false;
    token.disabled = submit.disabled = false;
    form.setAttribute("aria-busy", "false");
    status.textContent = message;
    doc.documentElement.dataset.viewerReady = state;
  }

  function lock(message) {
    generation += 1;
    sessionVerified = false;
    clearTimers();
    pending?.abort();
    pending = null;
    app?.dispose();
    app = null;
    appSuspended = false;
    retired ||= mounted;
    showGate(message);
  }

  // One bounded control request at a time. The body is either bounded status
  // JSON or an ignored login/logout response; never echo proxy error bodies.
  async function request(method, secret) {
    pending?.abort();
    const controller = new AbortController();
    pending = controller;
    const timeout = setTimer(() => controller.abort(), 10_000);
    try {
      if (method === "GET") {
        const body = await readSnapshot("auth/status", apiRoot,
          (url, init) => fetchImpl(url, { ...init, redirect: "error" }), controller.signal, 4096);
        const reply = JSON.parse(body);
        const session = reply?.data;
        if (reply?.ok !== true || typeof session?.required !== "boolean"
            || typeof session?.authenticated !== "boolean"
            || (!session.required && !session.authenticated)
            || (session.required && session.authenticated
                && (!Number.isSafeInteger(session.expires_at_ms) || session.expires_at_ms <= now()
                    || typeof session.session_epoch !== "string" || !/^[0-9a-f]{32}$/.test(session.session_epoch)))) {
          throw new Error("invalid session response");
        }
        return session;
      }
      let abortListener;
      const aborted = new Promise((_, reject) => {
        abortListener = () => reject(new Error("session request cancelled"));
        controller.signal.addEventListener("abort", abortListener, { once: true });
      });
      const work = (async () => {
        const response = await fetchImpl(new URL("auth/session", apiRoot), {
          method, credentials: "same-origin", cache: "no-store", redirect: "error",
          signal: controller.signal,
          headers: { "content-type": "application/json", accept: "application/json" },
          ...(method === "POST" ? { body: JSON.stringify({ token: secret }) } : {}),
        });
        void response.body?.cancel().catch(() => {});
        if (!response.ok || controller.signal.aborted) throw new Error("session request refused");
      })();
      try { await Promise.race([work, aborted]); }
      finally { controller.signal.removeEventListener("abort", abortListener); }
      return null;
    } finally {
      clearTimer(timeout);
      if (pending === controller) pending = null;
    }
  }

  function schedule(session) {
    clearTimers();
    if (!active) return;
    const expected = timerGeneration;
    const current = () => active && !disposed && timerGeneration === expected;
    if (session.required) expiryTimer = setTimer(() => {
      if (!current()) return;
      lock("Your session expired. Sign in again to reconnect.");
    }, Math.min(86_400_000, Math.max(1, session.expires_at_ms - now())));
    // Revalidate revocation from another tab and server restarts. This also
    // detects a server that has newly enabled authentication.
    pollTimer = setTimer(() => {
      if (!current()) return;
      pollTimer = null;
      void check();
    }, 15_000);
  }

  async function check() {
    if (!active || pending || disposed) return;
    const expected = generation;
    const expectedActivity = activityGeneration;
    try {
      const session = await request("GET");
      if (!active || generation !== expected || activityGeneration !== expectedActivity) return;
      required = session.required;
      if (!session.authenticated) {
        lock("Enter the access token configured by the operator to view this fleet.");
        return;
      }
      const nextEpoch = session.required ? session.session_epoch : null;
      if (mounted && sessionEpoch !== nextEpoch) {
        lock("Your session changed. Reloading the viewer with fresh state…");
        host.location.reload();
        return;
      }
      if (retired) {
        host.location.reload();
        return;
      }
      sessionEpoch = nextEpoch;
      sessionVerified = true;
      panel.hidden = true;
      element("inspect-toolbar").hidden = false;
      element("workspace").hidden = false;
      controls.hidden = !required;
      logout.disabled = false;
      if (!app) {
        const appEpoch = sessionEpoch;
        const appGeneration = generation;
        const current = () => active && sessionVerified && generation === appGeneration;
        const guardedFetch = async (url, init = {}) => {
          if (!current()) throw new Error("viewer session retired");
          const requestActivity = activityGeneration;
          const headers = new Headers(init.headers);
          if (appEpoch !== null) headers.set("x-ft-session-epoch", appEpoch);
          const response = await fetchImpl(url, { ...init, headers, redirect: "error" });
          if (!current() || activityGeneration !== requestActivity) {
            void response.body?.cancel().catch(() => {});
            throw new Error("viewer session retired");
          }
          if (response.status === 401 || response.status === 403) {
            lock("Your session is no longer authorized. Sign in again to reconnect.");
          }
          return response;
        };
        // Native EventSource retries reuse their original URL but automatically
        // pick up a rotated cookie. Bind every stream to this nonsecret epoch
        // so the server rejects an old viewer reconnecting under a new grant.
        function SessionEventSource(url, options) {
          if (!current()) throw new Error("viewer session retired");
          const sourceActivity = activityGeneration;
          const boundUrl = new URL(url, apiRoot);
          if (appEpoch !== null) boundUrl.searchParams.set("session_epoch", appEpoch);
          const source = new EventSourceImpl(boundUrl, options);
          if (appEpoch !== null) source.addEventListener("error", () => {
            if (current() && activityGeneration === sourceActivity) void check();
          });
          return source;
        }
        SessionEventSource.CLOSED = EventSourceImpl?.CLOSED ?? 2;
        app = mountApp({ document: doc, window: host, apiRoot, fetchImpl: guardedFetch,
          EventSourceImpl: SessionEventSource, managePageLifecycle: false,
          notificationScope: apiRoot.origin + apiRoot.pathname + "|" + (appEpoch ?? "local"),
          onNotificationsChanged(enabled) {
            backgroundNotifications = enabled === true && current();
            if (!backgroundNotifications && doc.visibilityState === "hidden") pause();
          },
          async verifyNotificationSession() {
            if (!current() || pending || disposed) return false;
            const expectedActivity = activityGeneration;
            try {
              const session = await request("GET");
              if (!current() || activityGeneration !== expectedActivity) return false;
              const nextEpoch = session.required ? session.session_epoch : null;
              if (!session.authenticated || nextEpoch !== appEpoch) {
                lock("Your session changed. Sign in again before inspecting this detection.");
                return false;
              }
              schedule(session);
              return true;
            } catch (_) {
              if (current() && activityGeneration === expectedActivity) {
                lock("The server's session status could not be verified. Retry when it is available.");
              }
              return false;
            }
          },
        });
        mounted = true;
      } else if (appSuspended && doc.visibilityState !== "hidden") {
        appSuspended = false;
        app.resume();
      }
      schedule(session);
    } catch (_) {
      if (active && generation === expected && activityGeneration === expectedActivity) {
        lock("The server's session status could not be verified. Retry when it is available.");
      }
    }
  }

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!active || pending || !required) return;
    const secret = token.value;
    token.value = "";
    if (!/^[\x21-\x7e]{32,256}$/.test(secret)) {
      status.textContent = "Enter the operator's 32–256 character access token.";
      return;
    }
    const expected = generation;
    const expectedActivity = activityGeneration;
    token.disabled = submit.disabled = true;
    form.setAttribute("aria-busy", "true");
    status.textContent = "Signing in…";
    try {
      await request("POST", secret);
      if (active && generation === expected && activityGeneration === expectedActivity) {
        channel?.postMessage({ type: "session-changed" });
        host.location.reload();
      }
    } catch (_) {
      if (active && generation === expected && activityGeneration === expectedActivity) {
        lock("Sign-in was not completed. Check the token and local server address, then retry.");
      }
    }
  });
  logout.addEventListener("click", async () => {
    if (!active) return;
    lock("Signing out…");
    const expected = generation;
    const expectedActivity = activityGeneration;
    try {
      await request("DELETE");
      if (!active || generation !== expected || activityGeneration !== expectedActivity) return;
      channel?.postMessage({ type: "signed-out" });
      status.textContent = "Signed out. Sign in to reconnect.";
    } catch (_) {
      if (active && generation === expected && activityGeneration === expectedActivity) {
        status.textContent = "The viewer is locked, but server sign-out could not be confirmed. Retry sign-out or wait for the session to expire.";
        controls.hidden = false;
      }
    }
  });
  retry.addEventListener("click", () => { void check(); });
  channel?.addEventListener("message", (event) => {
    if (required && ["signed-out", "session-changed"].includes(event.data?.type)) {
      lock("Your session changed in another tab. Check the connection or sign in to reconnect.");
    }
  });
  function pause() {
    if (!active) return;
    active = false;
    activityGeneration += 1;
    sessionVerified = false;
    clearTimers();
    pending?.abort();
    pending = null;
    if (app) { app.suspend(); appSuspended = true; }
    showGate("Checking your session before reconnecting…", "suspended");
  }
  function activate() {
    if (disposed || !pagePresent || doc.visibilityState === "hidden") return;
    if (active) {
      if (appSuspended) void check(); // Fresh status before exposing a background session.
      return;
    }
    active = true;
    activityGeneration += 1;
    void check();
  }
  // Without explicit notification opt-in visibility is a full network pause.
  // Opt-in retains only the detection feed and the same expiry/revocation checks;
  // rendering and pane output still wait for visible-session revalidation.
  // Navigation/BFCache is a terminal retirement even if the tab was hidden first.
  function suspend() {
    pagePresent = false;
    pause();
    lock("Checking your session before reconnecting…");
  }
  function resume() {
    pagePresent = true;
    activate();
  }
  host.addEventListener("pagehide", suspend);
  host.addEventListener("pageshow", resume);
  doc.addEventListener("visibilitychange", () => {
    if (doc.visibilityState === "hidden") {
      if (backgroundNotifications && active && sessionVerified && app) {
        app.suspend({ keepNotifications: true });
        appSuspended = true;
      } else pause();
    } else activate();
  });
  lock("Checking your session…");
  return { ready: check(), suspend, resume, dispose() { suspend(); disposed = true; channel?.close(); } };
}

if (typeof document !== "undefined") {
  try { mountAuthenticatedFleet(); } catch (_) {
    const errorBox = document.getElementById("error");
    errorBox.textContent = "FrankenTerm browser controls could not start. Reload this page to try again.";
    errorBox.hidden = false;
    document.documentElement.dataset.viewerReady = "error";
  }
}
