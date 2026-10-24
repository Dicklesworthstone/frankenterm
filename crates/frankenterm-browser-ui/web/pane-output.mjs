// Read-only captured output, not a terminal emulator or a pane-input transport.
// One owner binds a bounded transcript and its received cursor to one pane.
const U64_MAX = 18_446_744_073_709_551_615n;
const I64_MAX = 9_223_372_036_854_775_807n;
const RETRY_MAX_MS = 30_000;

function unsigned(value, maximum) {
  if (typeof value !== "string" || !/^[0-9]{1,20}$/.test(value)) return null;
  const integer = BigInt(value);
  return integer <= maximum ? integer.toString() : null;
}

// JSON.parse converts numeric u64/i64 identities to rounded Numbers. Validate
// the original JSON first, then quote integer tokens OUTSIDE strings. Parsing
// the original is important: quoting alone would accept invalid {1:2} JSON.
// Neither parse is attempted before the caller enforces the frame-size limit.
function decodeFrame(payload) {
  const frame = JSON.parse(payload);
  const exact = JSON.parse(payload.replace(
    /"(?:[^"\\]|\\.)*"|-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/g,
    (token) => /^-?\d+$/.test(token) ? JSON.stringify(token) : token,
  ));
  // Keep original types for schema/content validation. The integer-preserving
  // copy is ONLY identity evidence; a numeric content/code must not become an
  // accepted string just because the identity decoder quoted it.
  return { frame, exact };
}

function cursorFor(id, pane) {
  if (typeof id !== "string" || id.length > 64) return null;
  const match = /^delta-v1:pane:([0-9]{1,20}):([0-9]{1,19})$/.exec(id);
  if (!match || unsigned(match[1], U64_MAX) !== pane) return null;
  const segment = unsigned(match[2], I64_MAX);
  return segment !== null && segment !== "0" ? segment : null;
}

function safeSuffix(text, limit) {
  let start = Math.max(0, text.length - limit);
  // Never retain the second half of a UTF-16 surrogate pair on its own.
  if (start > 0 && /[\uDC00-\uDFFF]/.test(text[start])
      && /[\uD800-\uDBFF]/.test(text[start - 1])) start += 1;
  return text.slice(start);
}

export function createPaneOutput({
  apiRoot,
  EventSourceImpl = globalThis.EventSource,
  setTimer = globalThis.setTimeout,
  clearTimer = globalThis.clearTimeout,
  maxChars = 256 * 1024,
  maxChunks = 2048,
  maxFrameChars = 1024 * 1024,
} = {}) {
  for (const [limit, cap] of [[maxChars, 8 * 1024 * 1024], [maxChunks, 32768], [maxFrameChars, 8 * 1024 * 1024]]) {
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > cap) {
      throw new Error("invalid transcript limit");
    }
  }
  const root = new URL(apiRoot);
  let pane = null;
  let source = null;
  let timer = null;
  let generation = 0;
  let retryMs = 1000;
  let paused = false;
  let suspended = false;
  let history = false;
  let received = null;
  let status = "closed";
  let error = null;
  let gaps = 0;
  let unanchored = false;
  let discardedChars = 0;
  let chunks = [];
  let chars = 0;
  let dirty = true;
  let textDirty = false;
  let text = "";
  const eligible = () => pane !== null && !paused && !suspended;

  function retire() {
    generation += 1;
    if (timer !== null) clearTimer(timer);
    timer = null;
    const previous = source;
    source = null;
    if (previous) previous.close();
  }

  function append(content) {
    if (content.length === 0) return;
    let dropped = 0;
    const tail = safeSuffix(content, maxChars);
    dropped += content.length - tail.length;
    if (tail.length) {
      chunks.push(tail);
      chars += tail.length;
    }
    while (chunks.length > maxChunks) {
      const first = chunks.shift();
      chars -= first.length;
      dropped += first.length;
    }
    while (chars > maxChars) {
      const first = chunks[0];
      const keep = safeSuffix(first, Math.max(0, first.length - (chars - maxChars)));
      const removed = first.length - keep.length;
      if (keep.length) chunks[0] = keep;
      else chunks.shift();
      chars -= removed;
      dropped += removed;
    }
    discardedChars = Math.min(Number.MAX_SAFE_INTEGER, discardedChars + dropped);
    textDirty = true;
  }

  function reconnect(reason) {
    retire();
    error = reason;
    status = "retrying";
    dirty = true;
    if (!eligible()) return;
    if (received === null && !history) unanchored = true;
    const expected = generation;
    timer = setTimer(() => {
      if (generation !== expected || !eligible()) return;
      timer = null;
      open();
    }, retryMs);
    retryMs = Math.min(RETRY_MAX_MS, retryMs * 2);
  }

  function open() {
    if (!eligible() || source !== null || timer !== null) return;
    status = "connecting";
    dirty = true;
    const url = new URL("stream/deltas", root);
    url.searchParams.set("pane_id", pane);
    url.searchParams.set("max_hz", "50");
    if (received !== null) url.searchParams.set("since_id", received);
    else if (history) url.searchParams.set("since_id", "0");
    let candidate;
    try {
      candidate = new EventSourceImpl(url);
    } catch (_) {
      reconnect("output connection could not be opened");
      return;
    }
    source = candidate;
    const expected = generation;
    const selectedPane = pane;
    const current = () => eligible() && source === candidate && generation === expected;
    const frame = (message) => {
      if (!current()) return;
      try {
        if (typeof message.data !== "string" || message.data.length > maxFrameChars) {
          throw new Error("invalid frame size");
        }
        const { frame: value, exact } = decodeFrame(message.data);
        if (value?.schema !== "ft.stream.v1" || value.stream !== "deltas"
            || value.kind !== message.type || !value.data || typeof value.data !== "object"
            || Array.isArray(value.data)) throw new Error("invalid frame envelope");
        const data = value.data;
        if (message.type === "delta") {
          const segment = cursorFor(message.lastEventId, selectedPane);
          if (segment === null || typeof data.segment_id !== "number" || typeof data.pane_id !== "number"
              || unsigned(exact.data.segment_id, I64_MAX) !== segment
              || unsigned(exact.data.pane_id, U64_MAX) !== selectedPane || typeof data.content !== "string") {
            throw new Error("invalid delta identity or content");
          }
          // Reconnect may overlap with already admitted rows. Do not regress
          // the received cursor, append duplicates, or interpret ids as floats.
          if (received !== null && BigInt(segment) <= BigInt(received)) return;
          append(data.content);
          received = segment;
          status = "connected";
          error = null;
          retryMs = 1000;
        } else if (message.type === "ready") {
          if (typeof data.pane_id !== "number" || unsigned(exact.data.pane_id, U64_MAX) !== selectedPane) {
            throw new Error("wrong ready pane");
          }
          status = "connected";
          // A ready frame does not establish delivery or complete replay.
        } else if (message.type === "gap") {
          if (typeof data.pane_id !== "number" || unsigned(exact.data.pane_id, U64_MAX) !== selectedPane) {
            throw new Error("wrong gap pane");
          }
          gaps = Math.min(Number.MAX_SAFE_INTEGER, gaps + 1);
        } else if (message.type === "lag") {
          gaps = Math.min(Number.MAX_SAFE_INTEGER, gaps + 1);
        } else if (message.type === "error") {
          const code = data.code;
          reconnect(typeof code === "string" && /^[a-zA-Z0-9_.-]{1,80}$/.test(code)
            ? `output: ${code}` : "output: invalid server error frame");
          return;
        }
        dirty = true;
      } catch (_) {
        // Native EventSource advances its private cursor before invoking us.
        // Retire it on rejection so replacement uses only our accepted cursor.
        reconnect("output frame rejected; received cursor unchanged");
      }
    };
    for (const type of ["ready", "delta", "gap", "lag"]) candidate.addEventListener(type, frame);
    candidate.addEventListener("error", (message) => {
      if (!current()) return;
      if (typeof message.data === "string") frame(message);
      else if (candidate.readyState === EventSourceImpl.CLOSED) reconnect("output disconnected; reconnecting");
      else {
        status = "connecting";
        if (received === null && !history) unanchored = true;
        dirty = true;
      }
    });
  }

  function snapshot() {
    if (textDirty) {
      text = chunks.join("");
      textDirty = false;
    }
    return {
      paneId: pane, status, paused, suspended, history, cursor: received,
      text, discardedChars, gapNotifications: gaps, unanchored, error,
    };
  }

  return {
    // A new explicit selection is a new identity boundary, even for the same
    // numeric pane. Navigation elsewhere must not silently call this method.
    select(paneId, { retainedHistory = false } = {}) {
      const selected = unsigned(paneId, U64_MAX);
      if (selected === null) throw new Error("invalid pane id");
      retire();
      pane = selected;
      history = Boolean(retainedHistory);
      paused = false;
      received = null;
      chunks = [];
      chars = 0;
      text = "";
      textDirty = false;
      error = null;
      gaps = discardedChars = 0;
      unanchored = false;
      retryMs = 1000;
      status = suspended ? "suspended" : "connecting";
      dirty = true;
      open();
    },
    pause() {
      if (pane === null || paused) return;
      retire();
      paused = true;
      status = "paused";
      dirty = true;
    },
    resume() {
      if (pane === null || !paused) return;
      paused = false;
      if (received === null && !history) unanchored = true;
      status = suspended ? "suspended" : "connecting";
      dirty = true;
      open();
    },
    suspend() {
      if (suspended) return;
      retire();
      suspended = true;
      if (pane !== null) status = paused ? "paused" : "suspended";
      dirty = true;
    },
    restore() {
      if (!suspended) return;
      suspended = false;
      if (pane !== null && !paused && received === null && !history) unanchored = true;
      if (pane !== null) status = paused ? "paused" : "connecting";
      dirty = true;
      open();
    },
    close() {
      retire();
      pane = null;
      received = null;
      chunks = [];
      chars = 0;
      text = "";
      textDirty = false;
      error = null;
      paused = history = unanchored = false;
      gaps = discardedChars = 0;
      status = "closed";
      dirty = true;
    },
    snapshot,
    // The UI consumes at most one snapshot per animation frame, rather than
    // rebuilding the whole DOM for every chunk in an output flood.
    takeUpdate() {
      if (!dirty) return null;
      dirty = false;
      return snapshot();
    },
  };
}

// Bind native, accessible controls to the same controller exercised by tests.
// The selected fleet row is sampled only on an explicit Open action. No pane
// title or capture content is interpreted as markup, a link, or a VT command.
export function mountPaneOutput({
  document: doc = globalThis.document,
  apiRoot,
  getSelectedPane,
  output = createPaneOutput({ apiRoot }),
} = {}) {
  const element = (id) => {
    const node = doc.getElementById(id);
    if (!node) throw new Error(`missing output control: ${id}`);
    return node;
  };
  const workspace = element("workspace");
  const panel = element("pane-output");
  const title = element("pane-output-title");
  const statusLine = element("pane-output-status");
  const warnings = element("pane-output-warnings");
  const transcript = element("pane-output-text");
  const follow = element("pane-output-follow");
  const pause = element("pane-output-pause");
  const historyButton = element("pane-output-history");
  const closeButton = element("pane-output-close");
  const openButton = element("pane-output-open");
  const paneInput = element("pane-output-id");
  const paneForm = element("pane-output-form");
  const paneSubmit = element("pane-output-submit");
  const hint = element("pane-output-hint");
  const search = element("pane-output-search");
  const findForm = element("pane-output-find-form");
  const previous = element("pane-output-previous");
  const findStatus = element("pane-output-find-status");
  const canvas = element("terminal");
  let matchAt = -1;
  let matchQuery = "";
  let matchText = "";
  const setText = (node, value) => {
    // Avoid repeating live-region announcements on every output frame.
    if (node.textContent !== value) node.textContent = value;
  };

  function render() {
    const state = output.takeUpdate();
    if (!state) return;
    const opened = state.paneId !== null;
    panel.hidden = !opened;
    workspace.dataset.outputOpen = String(opened);
    setText(title, opened ? `Pane ${state.paneId} — captured output` : "Captured output");
    setText(statusLine, opened
      ? `${state.status} · read-only · ${state.history ? "following retained history" : "new output since opening"}`
      : "");
    setText(pause, state.paused ? "Resume" : "Pause");
    pause.setAttribute("aria-pressed", String(state.paused));
    const notices = [];
    if (state.discardedChars) notices.push("Older text was discarded from this bounded view.");
    if (state.gapNotifications) notices.push(`${state.gapNotifications} capture-gap or bus-lag notifications; continuity is not guaranteed.`);
    if (state.unanchored) notices.push("Reconnected before a received cursor existed; earlier output may be missing. Load retained history to recover what remains.");
    if (state.error) notices.push(state.error);
    setText(warnings, notices.join(" "));
    warnings.hidden = notices.length === 0;
    if (transcript.value !== state.text) {
      const scrollTop = transcript.scrollTop;
      const selectionStart = transcript.selectionStart;
      const selectionEnd = transcript.selectionEnd;
      transcript.value = state.text;
      if (follow.checked) transcript.scrollTop = transcript.scrollHeight;
      else {
        transcript.setSelectionRange(Math.min(selectionStart, state.text.length), Math.min(selectionEnd, state.text.length));
        transcript.scrollTop = scrollTop;
      }
      matchAt = -1;
      findStatus.textContent = "";
    }
  }

  function show(paneId, retainedHistory = false) {
    try {
      output.select(paneId, { retainedHistory });
    } catch (_) {
      hint.textContent = "Choose a pane or enter a valid unsigned pane ID.";
      return;
    }
    paneInput.value = output.snapshot().paneId;
    hint.textContent = "Output stays on this pane until you explicitly open another.";
    follow.checked = true;
    search.value = "";
    matchAt = -1;
    matchQuery = matchText = "";
    findStatus.textContent = "";
    render();
    transcript.focus();
  }

  function openSelected() {
    let paneId;
    try { paneId = getSelectedPane(); } catch (_) { /* finite hint below */ }
    if (typeof paneId !== "string") {
      hint.textContent = "Select a pane or detection row first, or enter a pane ID.";
      return;
    }
    show(paneId);
  }

  function close() {
    output.close();
    matchAt = -1;
    matchQuery = matchText = "";
    search.value = "";
    findStatus.textContent = "";
    hint.textContent = "Captured output is read-only; no keys are sent to a pane.";
    render();
    canvas.focus();
  }

  function find(backward = false) {
    const query = search.value;
    if (!query || query.length > 256) {
      findStatus.textContent = "Enter 1–256 characters to find.";
      return;
    }
    // Freeze admission while selecting text so subsequent output cannot move
    // the match. Resume is explicit and uses the last received segment cursor.
    output.pause();
    follow.checked = false;
    render();
    const currentText = transcript.value;
    const same = query === matchQuery && currentText === matchText;
    let found;
    if (backward) {
      const start = same && matchAt >= 0 ? matchAt - 1 : currentText.length;
      found = start < 0 ? -1 : currentText.lastIndexOf(query, start);
      if (found < 0) found = currentText.lastIndexOf(query);
    } else {
      found = currentText.indexOf(query, same && matchAt >= 0 ? matchAt + query.length : 0);
      if (found < 0) found = currentText.indexOf(query);
    }
    matchAt = found;
    matchQuery = query;
    matchText = currentText;
    findStatus.textContent = found < 0 ? "Not found in retained text. Stream paused."
      : "Match selected. Stream paused; Resume continues from the received cursor.";
    if (found >= 0) {
      transcript.focus();
      transcript.setSelectionRange(found, found + query.length);
    }
  }

  openButton.disabled = paneInput.disabled = paneSubmit.disabled = false;
  openButton.addEventListener("click", openSelected);
  paneForm.addEventListener("submit", (event) => {
    event.preventDefault();
    show(paneInput.value.trim());
  });
  pause.addEventListener("click", () => {
    if (output.snapshot().paused) output.resume();
    else output.pause();
    render();
  });
  historyButton.addEventListener("click", () => {
    const paneId = output.snapshot().paneId;
    if (paneId !== null) show(paneId, true);
  });
  closeButton.addEventListener("click", close);
  follow.addEventListener("change", () => {
    if (follow.checked) transcript.scrollTop = transcript.scrollHeight;
  });
  transcript.addEventListener("scroll", () => {
    if (transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight > 8) follow.checked = false;
  });
  findForm.addEventListener("submit", (event) => { event.preventDefault(); find(); });
  previous.addEventListener("click", () => find(true));
  panel.addEventListener("keydown", (event) => {
    if (event.key === "Escape") { event.preventDefault(); close(); }
  });
  render();
  return {
    render, openSelected, close,
    suspend() { output.suspend(); render(); },
    restore() { output.restore(); render(); },
  };
}
