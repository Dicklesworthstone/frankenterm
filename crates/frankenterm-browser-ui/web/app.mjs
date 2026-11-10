// FrankenTerm browser fleet viewer host.
//
// This page owns all network I/O and DOM events. The WASM FleetViewer owns
// state and renders frames; FrankenTermWeb paints their patches. The viewer is
// read-only: nothing here writes to a pane.
import { createPaneOutput, decodeFrame, mountPaneOutput } from "./pane-output.mjs";
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
// last accepted ready/event checkpoint in ?cursor. EventSource inherits that
// ID on live-only and control messages, which cannot advance this boundary.
// Retire a rejected source before its private cursor can skip the rejected
// frame on reconnect. A discontinuity requires an explicit new live view.
export function createFleetStream(viewer, {
  apiRoot,
  EventSourceImpl = globalThis.EventSource,
  setTimer = globalThis.setTimeout,
  clearTimer = globalThis.clearTimeout,
  onResync = () => {},
} = {}) {
  let source = null;
  let retryTimer = null;
  let retryMs = 1_000;
  let cursor = null;
  let terminalError = null;
  let stopped = true;
  let generation = 0;

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
      } else if (nextCursor !== null) {
        cursor = nextCursor;
        // A TCP open/ready followed by a replay failure is not recovery.
        retryMs = 1_000;
      }
      if (message.type === "ready" || message.type === "lag") resync();
    };
    for (const type of ["ready", "event", "lag"]) candidate.addEventListener(type, onFrame);
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
      if (frame.kind === "ready" || frame.kind === "event") streamError = null;
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
  setTimer = globalThis.setTimeout,
  clearTimer = globalThis.clearTimeout,
  setRepeating = globalThis.setInterval,
  clearRepeating = globalThis.clearInterval,
  requestFrame = (callback) => host.requestAnimationFrame(callback),
  cancelFrame = (handle) => host.cancelAnimationFrame(handle),
  ResizeObserverImpl = globalThis.ResizeObserver,
  now = Date.now,
} = {}) {
  const element = (id) => doc.getElementById(id);
  const viewport = element("fleet-viewport");
  const canvas = element("terminal");
  const screenText = element("screen-text");
  const modeLine = element("fleet-mode");
  const selectionLine = element("fleet-selection");
  const textToggle = element("fleet-text-toggle");
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
  let networkActive = false;
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
    textToggle.setAttribute("aria-pressed", String(asText));
    setText(textToggle, asText ? "Use canvas view" : "Use text view");
    const mode = viewer ? (asText ? "text" : "canvas") : modelFailed ? "capture-only" : "loading";
    doc.documentElement.dataset.viewerMode = mode;
    doc.documentElement.dataset.viewerReady = viewer ? "1" : modelFailed ? "capture-only" : "loading";
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
    updateMode();
  }

  function present() {
    // takeFlatPatches also drains StepProgram's output buffers in text mode.
    const { cells, spans } = viewer.takeFlatPatches();
    if (term && !preferText) {
      try {
        if (cells.length > 0) term.applyPatchBatchFlat(spans, cells);
        term.render();
      } catch (_) { retireRenderer(); }
    }
    mirrorText();
  }

  function stopFleet() {
    networkActive = false;
    if (refreshTimer !== null) clearRepeating(refreshTimer);
    refreshTimer = null;
    try { stream?.stop(); } finally { snapshots?.stop(); }
  }

  function failModel() {
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
    if (!active || !viewer || !snapshots || networkActive) return;
    networkActive = true;
    snapshots.start();
    stream.start();
    refresh();
    refreshTimer = setRepeating(refresh, REFRESH_MS);
  }

  const inspector = mountPaneOutput({
    document: doc, apiRoot,
    output: createPaneOutput({ apiRoot, EventSourceImpl, setTimer, clearTimer }),
    restoreFocus: () => {
      if (active) (viewer ? (textView() ? screenText : canvas) : element("pane-output-id")).focus();
    },
    getSelectedPane: () => {
      // Apply queued navigation before sampling its identity. Once opened,
      // later row changes never retarget this pane's output subscription.
      advance();
      return viewer?.selectedPaneId();
    },
  });
  const historySearch = mountHistorySearch({
    document: doc,
    inspectCapture: (hit) => inspector.openCapture(hit),
    readBody: (path, signal) => readSnapshot(path, apiRoot, fetchImpl, signal, 8 * 1024 * 1024),
    inspectPane: (paneId) => {
      // Route the exact captured identity through the same validated native
      // form as a manual Open action, never through the mutable fleet row.
      element("pane-output-id").value = paneId;
      element("pane-output-form").requestSubmit();
    },
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
  if (ResizeObserverImpl) new ResizeObserverImpl(resize).observe(viewport);
  else host.addEventListener("resize", resize);

  function frame() {
    frameHandle = null;
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
    frameHandle = requestFrame(frame);
  }
  function resume() {
    if (active) return;
    active = true;
    inspector.restore();
    resize();
    startFleet();
    frameHandle = requestFrame(frame);
  }
  function suspend() {
    if (!active) return;
    active = false;
    stopFleet();
    inspector.suspend();
    historySearch.suspend();
    if (frameHandle !== null) cancelFrame(frameHandle);
    frameHandle = null;
  }
  host.addEventListener("pagehide", suspend);
  host.addEventListener("pageshow", resume);
  updateMode();
  resume();

  const modelReady = Promise.resolve().then(loadViewer).then(async ({ default: initViewer, FleetViewer }) => {
    await initViewer();
    viewer = new FleetViewer(80, 24);
    viewer.setTime(now());
    viewer.init();
    present();
    snapshots = createFleetRefresh(viewer, { apiRoot, fetchImpl, setTimer, clearTimer });
    stream = createFleetStream(snapshots, { apiRoot, EventSourceImpl, setTimer, clearTimer, onResync: refresh });
    updateMode();
    resize();
    startFleet();
    return true;
  }).catch(() => { failModel(); return false; });

  const rendererReady = Promise.resolve().then(loadRenderer).then(async ({ default: initRenderer, FrankenTermWeb }) => {
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
  return { ready: Promise.all([modelReady, rendererReady]), suspend, resume };
}

if (typeof document !== "undefined") {
  try { mountFleetApp(); } catch (_) {
    const errorBox = document.getElementById("error");
    errorBox.textContent = "FrankenTerm browser controls could not start. Reload this page to try again.";
    errorBox.hidden = false;
    document.documentElement.dataset.viewerReady = "error";
  }
}
