// FrankenTerm browser fleet viewer host.
//
// This page owns all network I/O and DOM events. The WASM FleetViewer owns
// state and renders frames; FrankenTermWeb paints their patches. The viewer is
// read-only: nothing here writes to a pane.
import initRenderer, { FrankenTermWeb } from "./pkg/FrankenTerm.js";
import initViewer, { FleetViewer } from "./pkg/ft_fleet_viewer.js";

// The package is served under <ft web root>/ui/, beside the read API.
const API_ROOT = new URL("..", window.location.href);
const REFRESH_MS = 15_000;
const CLOCK_MS = 1_000;
const STREAM_RETRY_MAX_MS = 30_000;
const STREAM = { DISCONNECTED: 0, CONNECTING: 1, LIVE: 2 };
const FORWARDED_NAV_KEYS = new Set(["Tab", "ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown"]);

const container = document.getElementById("container");
const canvas = document.getElementById("terminal");
const screenText = document.getElementById("screen-text");
const errorBox = document.getElementById("error");

function showError(message) {
  errorBox.textContent = message;
  errorBox.hidden = false;
}

async function fetchText(path) {
  const response = await fetch(new URL(path, API_ROOT), {
    cache: "no-store",
    credentials: "same-origin",
  });
  const body = await response.text();
  // ft web reports failures as an ok:false JSON envelope, which the viewer
  // decodes and shows; anything else (a proxy page, a bare status) is ours to say.
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
  await Promise.all([initRenderer(), initViewer()]);
  await document.fonts.load('16px "Pragmasevka NF"').catch(() => undefined);

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
      [panes, events] = await Promise.all([fetchText("panes"), fetchText("events?limit=200")]);
    } catch (error) {
      viewer.reportError(`refresh failed: ${error.message}`);
      return;
    }
    // A body the viewer cannot read is shown in its status line; keep going.
    try { viewer.ingestPanes(panes); } catch (_) { /* reported by the viewer */ }
    try { viewer.ingestEvents(events); } catch (_) { /* reported by the viewer */ }
  }

  // EventSource reconnects by itself and sends Last-Event-ID, so the server
  // replays what was missed; the viewer drops replayed duplicates. A source the
  // browser gave up on (CLOSED) is reopened here with backoff.
  let retryMs = 1_000;
  function openStream() {
    viewer.setStreamState(STREAM.CONNECTING);
    const source = new EventSource(new URL("stream/events?channel=all&max_hz=20", API_ROOT));
    const onFrame = (message) => {
      try { viewer.ingestStreamFrame(message.data); } catch (_) { /* reported by the viewer */ }
    };
    for (const type of ["ready", "event", "lag"]) source.addEventListener(type, onFrame);
    source.addEventListener("open", () => { retryMs = 1_000; });
    source.addEventListener("error", () => {
      if (source.readyState === EventSource.CLOSED) {
        viewer.setStreamState(STREAM.DISCONNECTED);
        setTimeout(() => { refresh(); openStream(); }, retryMs);
        retryMs = Math.min(STREAM_RETRY_MAX_MS, retryMs * 2);
      } else {
        viewer.setStreamState(STREAM.CONNECTING);
      }
    });
  }

  window.addEventListener("keydown", (event) => {
    // Browser and OS shortcuts stay with the browser.
    if (event.metaKey || event.ctrlKey) return;
    try {
      term.input(keyInput(event, "down"));
    } finally {
      for (const json of term.drainEncodedInputs()) viewer.pushEncodedInput(json);
      // The renderer also queues VT bytes and IME traces; this viewer sends
      // nothing to a terminal, so discard them rather than let them grow.
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

  let lastClock = 0;
  function frame() {
    const now = Date.now();
    if (now - lastClock >= CLOCK_MS) {
      viewer.setTime(now);
      lastClock = now;
    }
    if (viewer.takeRefreshRequest()) refresh();
    if (viewer.step()) present();
    requestAnimationFrame(frame);
  }

  await refresh();
  openStream();
  setInterval(refresh, REFRESH_MS);
  canvas.focus();
  requestAnimationFrame(frame);
  document.documentElement.dataset.viewerReady = "1";
}

main().catch((error) => {
  showError(`FrankenTerm fleet viewer failed to start: ${error?.message ?? error}`);
  document.documentElement.dataset.viewerReady = "error";
});
