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
