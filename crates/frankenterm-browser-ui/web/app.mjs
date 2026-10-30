// FrankenTerm browser fleet viewer host.
//
// This page owns all network I/O and DOM events. The WASM FleetViewer owns
// state and renders frames; FrankenTermWeb paints their patches. The viewer is
// read-only: nothing here writes to a pane.
import { mountPaneOutput } from "./pane-output.mjs";
import { mountHistorySearch } from "./history-panel.mjs";

const REFRESH_MS = 15_000;
const CLOCK_MS = 1_000;
const STREAM_RETRY_MAX_MS = 30_000;
const STREAM = { DISCONNECTED: 0, CONNECTING: 1, LIVE: 2 };
const FORWARDED_NAV_KEYS = new Set(["ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown"]);

// Keep persisted i64 IDs as strings: converting through Number corrupts IDs
// above 2^53. This is the /stream/events cursor, not a delta-stream cursor.
function validEventId(id) {
  return typeof id === "string" && /^[0-9]{1,19}$/.test(id)
    && BigInt(id) <= 9_223_372_036_854_775_807n;
}

function streamErrorMessage(payload) {
  // Do not echo arbitrary server/proxy text or allocate a second huge object.
  if (payload.length > 4096) return "stream: invalid server error frame";
  try {
    const frame = JSON.parse(payload);
    const code = frame?.data?.code;
    if (frame.schema === "ft.stream.v1" && frame.kind === "error"
        && typeof code === "string" && /^[a-zA-Z0-9_.-]{1,80}$/.test(code)) {
      return `stream: ${code}`;
    }
  } catch (_) { /* finite diagnostic below */ }
  return "stream: invalid server error frame";
}

// One owner for the EventSource and its replacement timer. Native transient
// reconnects retain their own Last-Event-ID; a replacement source must carry
// our last successfully ingested ID explicitly in since_id. Rejected frames
// never acknowledge an ID, even though EventSource has already advanced its
// private cursor before invoking our listener.
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
  let lastEventId = "";
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

  function reconnect(message) {
    const previous = source;
    source = null;
    if (previous) previous.close();
    if (stopped) return;
    viewer.setStreamState(STREAM.DISCONNECTED);
    if (message) viewer.reportError(message);
    if (retryTimer !== null) return;
    const expectedGeneration = generation;
    retryTimer = setTimer(() => {
      if (stopped || generation !== expectedGeneration) return;
      retryTimer = null;
      open();
    }, retryMs);
    retryMs = Math.min(STREAM_RETRY_MAX_MS, retryMs * 2);
  }

  function open() {
    if (stopped || source !== null) return;
    viewer.setStreamState(STREAM.CONNECTING);
    const url = new URL("stream/events?channel=all&max_hz=20", apiRoot);
    if (lastEventId !== "") url.searchParams.set("since_id", lastEventId);
    let candidate;
    try {
      candidate = new EventSourceImpl(url);
    } catch (_) {
      reconnect("stream: connection could not be opened");
      return;
    }
    source = candidate;
    const current = () => !stopped && source === candidate;
    const onFrame = (message) => {
      if (!current()) return;
      const id = message.lastEventId ?? "";
      if (message.type === "event" && id !== "" && !validEventId(id)) {
        // No accepted boundary exists yet: recover from the beginning rather
        // than silently stepping over an unprocessed first persisted event.
        if (lastEventId === "") lastEventId = "0";
        reconnect("stream: invalid persisted event cursor");
        return;
      }
      try {
        viewer.ingestStreamFrame(message.data);
      } catch (_) {
        if (lastEventId === "") lastEventId = "0";
        reconnect("stream: frame rejected; retrying from the last accepted cursor");
        return;
      }
      if (message.type === "event") {
        if (id !== "") lastEventId = id;
        // A TCP open/ready followed by a replay failure is not recovery.
        retryMs = 1_000;
      }
      if (message.type === "ready" || message.type === "lag") resync();
    };
    for (const type of ["ready", "event", "lag"]) candidate.addEventListener(type, onFrame);
    candidate.addEventListener("error", (message) => {
      if (!current()) return;
      // Named SSE `event: error` is a MessageEvent, whereas a network error
      // is an Event without data. Do not discard replay/storage failures as
      // if they were merely an EventSource transport state transition.
      if (typeof message.data === "string") {
        if (lastEventId === "") lastEventId = "0";
        reconnect(streamErrorMessage(message.data));
      } else if (candidate.readyState === EventSourceImpl.CLOSED) {
        reconnect("stream: disconnected; reconnecting");
      } else {
        viewer.setStreamState(STREAM.CONNECTING);
      }
    });
  }

  return {
    start() {
      if (!stopped) return;
      stopped = false;
      generation += 1;
      open();
    },
    stop() {
      stopped = true;
      generation += 1;
      if (retryTimer !== null) clearTimer(retryTimer);
      retryTimer = null;
      const previous = source;
      source = null;
      if (previous) previous.close();
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
  if (event.target !== canvas || event.metaKey || event.ctrlKey || event.altKey || event.key === "Tab") return;
  if (event.key === "Enter") {
    event.preventDefault();
    if (!event.repeat) inspector.openSelected();
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

async function main() {
  // Dynamic imports keep the actual transport owner testable without loading
  // a generated WASM bundle, DOM, renderer or network in the Node harness.
  const [{ default: initRenderer, FrankenTermWeb }, { default: initViewer, FleetViewer }] =
    await Promise.all([import("./pkg/FrankenTerm.js"), import("./pkg/ft_fleet_viewer.js")]);
  await Promise.all([initRenderer(), initViewer()]);
  await document.fonts.load('16px "Pragmasevka NF"').catch(() => undefined);

  const apiRoot = new URL("..", window.location.href);
  const container = document.getElementById("container");
  const canvas = document.getElementById("terminal");
  const screenText = document.getElementById("screen-text");
  const term = new FrankenTermWeb();
  const contract = typeof term.apiContract === "function" ? term.apiContract() : null;
  if (contract?.apiLine !== "frankenterm-js" || contract.apiVersion !== "1.0.0") {
    throw new Error("unsupported FrankenTerm web renderer API contract");
  }
  let dpr = window.devicePixelRatio || 1;
  await term.init(canvas, { cols: 80, rows: 24, cellWidth: 8, cellHeight: 16, dpr, focused: true });
  let geometry = term.fitToContainer(container.clientWidth, container.clientHeight, dpr);
  const viewer = new FleetViewer(geometry.cols, geometry.rows);
  viewer.setTime(Date.now());
  viewer.init();

  function present() {
    const { cells, spans } = viewer.takeFlatPatches();
    if (cells.length > 0) term.applyPatchBatchFlat(spans, cells);
    term.render();
    screenText.textContent = viewer.screenText();
  }
  present();

  const snapshots = createFleetRefresh(viewer, { apiRoot });
  const refresh = snapshots.refresh;
  const stream = createFleetStream(snapshots, { apiRoot, onResync: refresh });
  const inspector = mountPaneOutput({
    apiRoot,
    getSelectedPane: () => {
      // Apply queued navigation before sampling its identity. Once opened,
      // later row changes never retarget this pane's output subscription.
      if (viewer.step()) present();
      return viewer.selectedPaneId();
    },
  });
  const historySearch = mountHistorySearch({
    inspectCapture: (hit) => inspector.openCapture(hit),
    readBody: (path, signal) => readSnapshot(path, apiRoot, globalThis.fetch, signal, 8 * 1024 * 1024),
    inspectPane: (paneId) => {
      // Route the exact captured identity through the same validated native
      // form as a manual Open action, never through the mutable fleet row.
      document.getElementById("pane-output-id").value = paneId;
      document.getElementById("pane-output-form").requestSubmit();
    },
  });

  window.addEventListener("keydown", (event) => {
    handleFleetKey(event, { canvas, term, viewer, inspector });
  });

  new ResizeObserver(() => {
    if (container.clientWidth <= 0 || container.clientHeight <= 0) return;
    dpr = window.devicePixelRatio || 1;
    geometry = term.fitToContainer(container.clientWidth, container.clientHeight, dpr);
    viewer.resize(geometry.cols, geometry.rows);
  }).observe(container);

  let active = false;
  let refreshTimer = null;
  let frameHandle = null;
  let lastClock = 0;
  function frame() {
    frameHandle = null;
    if (!active) return;
    const now = Date.now();
    if (now - lastClock >= CLOCK_MS) {
      viewer.setTime(now);
      lastClock = now;
    }
    if (viewer.takeRefreshRequest()) refresh();
    if (viewer.step()) present();
    inspector.render();
    frameHandle = requestAnimationFrame(frame);
  }
  function resume() {
    if (active) return;
    active = true;
    snapshots.start();
    stream.start();
    inspector.restore();
    refresh();
    refreshTimer = setInterval(refresh, REFRESH_MS);
    frameHandle = requestAnimationFrame(frame);
  }
  function suspend() {
    active = false;
    stream.stop();
    snapshots.stop();
    inspector.suspend();
    historySearch.suspend();
    if (refreshTimer !== null) clearInterval(refreshTimer);
    if (frameHandle !== null) cancelAnimationFrame(frameHandle);
    refreshTimer = frameHandle = null;
  }
  window.addEventListener("pagehide", suspend);
  window.addEventListener("pageshow", resume);
  resume();
  canvas.focus();
  document.documentElement.dataset.viewerReady = "1";
}

if (typeof document !== "undefined") {
  main().catch((error) => {
    const errorBox = document.getElementById("error");
    errorBox.textContent = `FrankenTerm fleet viewer failed to start: ${error?.message ?? error}`;
    errorBox.hidden = false;
    document.documentElement.dataset.viewerReady = "error";
  });
}
