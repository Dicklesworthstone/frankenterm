// FrankenTerm browser fleet viewer host.
//
// This page owns all network I/O and DOM events. The WASM FleetViewer owns
// state and renders frames; FrankenTermWeb paints their patches. The viewer is
// read-only: nothing here writes to a pane.

const REFRESH_MS = 15_000;
const CLOCK_MS = 1_000;
const STREAM_RETRY_MAX_MS = 30_000;
const STREAM = { DISCONNECTED: 0, CONNECTING: 1, LIVE: 2 };
const FORWARDED_NAV_KEYS = new Set(["Tab", "ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown"]);

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
    try {
      Promise.resolve(onResync()).catch(() => {
        if (!stopped) viewer.reportError("stream: snapshot recovery failed");
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

async function fetchText(path, apiRoot) {
  const response = await fetch(new URL(path, apiRoot), {
    cache: "no-store",
    credentials: "same-origin",
  });
  const body = await response.text();
  if (!response.ok && !body.trimStart().startsWith("{")) {
    throw new Error(`${path}: HTTP ${response.status}`);
  }
  return body;
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

  async function refresh() {
    let panes;
    let events;
    try {
      [panes, events] = await Promise.all([
        fetchText("panes", apiRoot), fetchText("events?limit=200", apiRoot),
      ]);
    } catch (error) {
      viewer.reportError(`refresh failed: ${error.message}`);
      return;
    }
    try { viewer.ingestPanes(panes); } catch (_) { /* reported by the viewer */ }
    try { viewer.ingestEvents(events); } catch (_) { /* reported by the viewer */ }
  }
  const stream = createFleetStream(viewer, { apiRoot, onResync: refresh });

  window.addEventListener("keydown", (event) => {
    if (event.metaKey || event.ctrlKey) return;
    try {
      term.input(keyInput(event, "down"));
    } finally {
      for (const json of term.drainEncodedInputs()) viewer.pushEncodedInput(json);
      term.drainEncodedInputBytes();
      term.drainImeCompositionJsonl("fleet-viewer", 0n, "");
    }
    if (FORWARDED_NAV_KEYS.has(event.key)) event.preventDefault();
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
    frameHandle = requestAnimationFrame(frame);
  }
  function resume() {
    if (active) return;
    active = true;
    stream.start();
    refresh();
    refreshTimer = setInterval(refresh, REFRESH_MS);
    frameHandle = requestAnimationFrame(frame);
  }
  function suspend() {
    active = false;
    stream.stop();
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
