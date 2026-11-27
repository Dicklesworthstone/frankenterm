// Read-only captured output, not a terminal emulator or a pane-input transport.
// One owner binds a bounded transcript and its received cursor to one pane.
const U64_MAX = 18_446_744_073_709_551_615n;
const I64_MAX = 9_223_372_036_854_775_807n;
const RETRY_MAX_MS = 30_000;
const RECENT_SEGMENTS = 32;

function unsigned(value, maximum) {
  if (typeof value !== "string" || !/^[0-9]{1,20}$/.test(value)) return null;
  const integer = BigInt(value);
  return integer <= maximum ? integer.toString() : null;
}

// A delta's capture UUID, or null when the server knows none (older servers
// omit the field). Anything else is invalid provenance, not "unknown".
function captureUuidOf(data) {
  const value = data.pane_uuid;
  if (value === undefined || value === null) return null;
  if (typeof value === "string" && /^[0-9a-f]{32}$/.test(value)) return value;
  throw new Error("invalid delta capture provenance");
}

// JSON.parse converts numeric u64/i64 identities to rounded Numbers. Validate
// the original JSON first, then quote integer tokens OUTSIDE strings. Parsing
// the original is important: quoting alone would accept invalid {1:2} JSON.
// Neither parse is attempted before the caller enforces the frame-size limit.
export function decodeFrame(payload) {
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

function cursorFor(id, pane, allowZero = false) {
  if (typeof id !== "string" || id.length > 128) return null;
  const match = /^delta-v2:([0-9a-f]{32}):pane:(0|[1-9][0-9]{0,19}):(0|[1-9][0-9]{0,18})$/.exec(id);
  if (!match || match[0] !== id || unsigned(match[2], U64_MAX) !== pane) return null;
  const segment = unsigned(match[3], I64_MAX);
  return segment !== null && (allowZero || segment !== "0")
    ? { token: id, epoch: match[1], segment } : null;
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
  captureTimeoutMs = 10_000,
  readyTimeoutMs = 10_000,
} = {}) {
  for (const [limit, cap] of [[maxChars, 8 * 1024 * 1024], [maxChunks, 32768], [maxFrameChars, 8 * 1024 * 1024], [captureTimeoutMs, 60_000], [readyTimeoutMs, 60_000]]) {
    if (!Number.isSafeInteger(limit) || limit < 1 || limit > cap) {
      throw new Error("invalid transcript limit");
    }
  }
  const root = new URL(apiRoot);
  let pane = null;
  let source = null;
  let timer = null;
  let readyTimer = null;
  let readyGeneration = 0;
  let captureTimer = null;
  let capture = null;
  let followFromSegment = null;
  // The capture UUID of the admitted historical segment, kept while
  // following it: a reused pane ID continues seq but gets a new UUID.
  let captureProvenance = null;
  let capturePending = false;
  let generation = 0;
  let retryMs = 1000;
  let paused = false;
  let suspended = false;
  let history = false;
  let recent = false;
  let received = null;
  let receivedCursor = null;
  let receivedSequence = null;
  let sequenceDiscontinuities = 0;
  let sequenceRestarted = false;
  let retainedPrefixMissing = false;
  let status = "closed";
  let error = null;
  let terminal = false;
  let gaps = 0;
  let unanchored = false;
  let discardedChars = 0;
  let chunks = [];
  let chars = 0;
  let dirty = true;
  let textDirty = false;
  let text = "";
  const eligible = () => pane !== null && !terminal && !paused && !suspended && (capture === null || capturePending);

  function clearReadyDeadline() {
    readyGeneration += 1;
    if (readyTimer !== null) clearTimer(readyTimer);
    readyTimer = null;
  }

  function awaitReady(current) {
    // Historical capture already has a total delivery deadline. Live streams
    // need a bounded initial/native-reconnect handshake, not an idle timeout:
    // EventSource does not expose the server's comment-only keepalives.
    if (capture !== null || readyTimer !== null) return;
    const expected = ++readyGeneration;
    readyTimer = setTimer(() => {
      if (!current() || readyGeneration !== expected) return;
      readyTimer = null;
      reconnect("output connection timed out before becoming ready; reconnecting");
    }, readyTimeoutMs);
  }

  function retire() {
    generation += 1;
    clearReadyDeadline();
    if (timer !== null) clearTimer(timer);
    timer = null;
    if (captureTimer !== null) clearTimer(captureTimer);
    captureTimer = null;
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

  function settleCapture(reason, available = false) {
    retire();
    capturePending = false;
    status = available ? "historical" : "unavailable";
    error = reason;
    dirty = true;
  }

  function reconnect(reason) {
    // A historical read is a finite request, not a live subscription. Never
    // fall forward to another segment or let EventSource retry it implicitly.
    if (capture !== null) {
      settleCapture(reason);
      return;
    }
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

  function halt(reason) {
    retire();
    terminal = true;
    capturePending = false;
    status = capture === null ? "discontinuous" : "unavailable";
    error = capture === null
      ? `${reason}. Open a pane or load retained history to choose the current capture history.`
      : `${reason}. Search again to choose a capture from the current history.`;
    dirty = true;
  }

  function open() {
    if (!eligible() || source !== null || timer !== null) return;
    status = capture === null ? "connecting" : "loading";
    dirty = true;
    const url = new URL("stream/deltas", root);
    url.searchParams.set("pane_id", pane);
    url.searchParams.set("max_hz", "50");
    if (capture !== null) url.searchParams.set("cursor", capture.captureCursor);
    else if (receivedCursor !== null) url.searchParams.set("cursor", receivedCursor.token);
    else if (history) url.searchParams.set("since_id", "0");
    else if (recent) url.searchParams.set("recent", String(RECENT_SEGMENTS));
    let candidate;
    try {
      candidate = new EventSourceImpl(url);
    } catch (_) {
      reconnect("output connection could not be opened");
      return;
    }
    source = candidate;
    let ready = false;
    const expected = generation;
    const selectedPane = pane;
    const expectedCaptureCursor = capture === null ? null : cursorFor(capture.captureCursor, selectedPane, true);
    const current = () => eligible() && source === candidate && generation === expected;
    awaitReady(current);
    if (capture !== null) {
      captureTimer = setTimer(() => {
        if (current()) settleCapture("Historical capture was not received before the deadline. Search again to check retained history.");
      }, captureTimeoutMs);
    }
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
        if (message.type === "cursor_discontinuity") {
          const boundary = cursorFor(data.resume_cursor, selectedPane, true);
          const accepted = receivedCursor ?? expectedCaptureCursor;
          const valid = data.code === "delta_cursor_discontinuity" && data.terminal === true
            && ["cursor_epoch_mismatch", "cursor_ahead_of_high_water", "high_water_regressed"].includes(data.reason)
            && boundary !== null && (accepted === null
              || (boundary.epoch === accepted.epoch && (capture !== null
                ? boundary.segment === accepted.segment : BigInt(boundary.segment) <= BigInt(accepted.segment))));
          halt(valid ? `output history changed (${data.reason})` : "output: invalid capture history discontinuity");
          return;
        }
        if (!ready && message.type !== "ready" && message.type !== "error") {
          reconnect("output arrived before its replay boundary was confirmed");
          return;
        }
        if (["gap", "lag", "error"].includes(message.type) && message.lastEventId) {
          const inherited = cursorFor(message.lastEventId, selectedPane, true);
          if (inherited === null || receivedCursor === null) throw new Error("invalid inherited cursor");
          if (inherited.epoch !== receivedCursor.epoch) {
            halt("output capture database changed");
            return;
          }
          if (BigInt(inherited.segment) > BigInt(receivedCursor.segment)) throw new Error("unadmitted control cursor");
        }
        if (message.type === "delta") {
          const incoming = cursorFor(message.lastEventId, selectedPane);
          const segment = incoming?.segment ?? null;
          const sequence = unsigned(exact.data.seq, U64_MAX);
          if (segment === null || typeof data.segment_id !== "number" || typeof data.pane_id !== "number"
              || unsigned(exact.data.segment_id, I64_MAX) !== segment
              || unsigned(exact.data.pane_id, U64_MAX) !== selectedPane
              || typeof data.seq !== "number" || sequence === null || typeof data.content !== "string") {
            throw new Error("invalid delta identity or content");
          }
          const paneUuid = captureUuidOf(data);
          if (receivedCursor === null || incoming.epoch !== receivedCursor.epoch) {
            halt("output capture database changed");
            return;
          }
          if (capture !== null) {
            if (segment !== capture.segmentId) {
              settleCapture("The requested capture is no longer available. Later pane output was not opened.");
              return;
            }
            if (typeof data.captured_at !== "number" || typeof data.content_len !== "number"
                || (capture.paneUuid !== undefined && paneUuid !== capture.paneUuid)
                || unsigned(exact.data.captured_at, I64_MAX) !== capture.capturedAt
                || unsigned(exact.data.content_len, U64_MAX) !== capture.contentLength) {
              settleCapture("Capture identity changed or was invalid. Search again; no current pane was opened.");
              return;
            }
            // content_len is the stored length, not the redacted wire length.
            // Admit only the server's currently redacted text, then stop before
            // any following segment can arrive, even for a reused pane ID.
            append(data.content);
            received = segment;
            receivedCursor = incoming;
            receivedSequence = sequence;
            captureProvenance = paneUuid;
            settleCapture(null, true);
            return;
          }
          // Reconnect may overlap with already admitted rows. Do not regress
          // the received cursor, append duplicates, or interpret ids as floats.
          if (received !== null && BigInt(segment) <= BigInt(received)) {
            status = "connected";
            dirty = true;
            return;
          }
          // A reused pane ID continues the per-pane seq, so only the capture
          // UUID shows that later output came from another terminal. A
          // followed capture always has a known UUID (followCapture refuses
          // an unknown one), so a different or missing UUID is another capture.
          if (followFromSegment !== null && paneUuid !== captureProvenance) {
            halt("another capture reused this pane ID after the selected segment");
            return;
          }
          // Global segment ids interleave panes; only this pane's stored seq
          // can reveal an omitted range. Keep the last admitted sequence over
          // disconnect, pause and suspension: ephemeral bus gap notifications
          // cannot reveal rows removed by retention while we were away.
          if (receivedSequence !== null) {
            const previous = BigInt(receivedSequence);
            const current = BigInt(sequence);
            if (current !== previous + 1n) {
              if (followFromSegment !== null) {
                halt("capture sequence changed after the selected segment");
                return;
              }
              sequenceDiscontinuities = Math.min(Number.MAX_SAFE_INTEGER, sequenceDiscontinuities + 1);
              if (current <= previous) sequenceRestarted = true;
            }
          } else if (history && sequence !== "0") {
            // Explicit retained-history reads start at the beginning of what
            // remains, which is not necessarily the beginning of capture.
            retainedPrefixMissing = true;
          }
          append(data.content);
          received = segment;
          receivedCursor = incoming;
          receivedSequence = sequence;
          status = "connected";
          error = null;
          retryMs = 1000;
        } else if (message.type === "ready") {
          if (typeof data.pane_id !== "number" || unsigned(exact.data.pane_id, U64_MAX) !== selectedPane) {
            throw new Error("wrong ready pane");
          }
          // Ready establishes the fresh tail even before the first delta.
          // Validate the scoped wire cursor as well as the exact JSON ID:
          // native reconnect may use Last-Event-ID from this checkpoint.
          const expectedCursor = receivedCursor ?? expectedCaptureCursor;
          if (expectedCursor === null && recent && data.recent_segments !== RECENT_SEGMENTS) {
            halt("the server did not confirm the requested recent output window");
            return;
          }
          const expectedResume = expectedCursor?.segment ?? url.searchParams.get("since_id");
          const resume = unsigned(exact.data.resume_after_id, I64_MAX);
          const checkpoint = cursorFor(message.lastEventId, selectedPane, true);
          const validCheckpoint = typeof data.resume_after_id === "number" && resume !== null
            && checkpoint?.segment === resume && data.cursor === message.lastEventId;
          if (validCheckpoint && expectedCursor !== null && checkpoint.epoch !== expectedCursor.epoch) {
            halt("output capture database changed");
            return;
          }
          const safeResume = validCheckpoint
            && (expectedResume === null
              || (capture !== null ? resume === expectedResume : BigInt(resume) <= BigInt(expectedResume)));
          if (!safeResume) {
            reconnect("output replay boundary changed; received cursor unchanged");
            return;
          }
          // Preserve a validated baseline across pause, suspension and owned
          // replacement without pretending that any transcript was delivered.
          // A finite historical capture acknowledges only its requested row.
          if (capture === null && received === null) received = resume;
          if (receivedCursor === null) receivedCursor = checkpoint;
          ready = true;
          clearReadyDeadline();
          status = capture === null ? "connected" : "loading";
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
        if (message.type === "cursor_discontinuity") {
          halt("output: invalid capture history discontinuity");
          return;
        }
        // Native EventSource advances its private cursor before invoking us.
        // Retire it on rejection so replacement uses only our accepted cursor.
        reconnect("output frame rejected; received cursor unchanged");
      }
    };
    for (const type of ["ready", "delta", "gap", "lag", "cursor_discontinuity"]) candidate.addEventListener(type, frame);
    candidate.addEventListener("error", (message) => {
      if (!current()) return;
      if (typeof message.data === "string") frame(message);
      else if (capture !== null) reconnect("Historical capture connection ended before delivery. Search again to retry.");
      else if (candidate.readyState === EventSourceImpl.CLOSED) reconnect("output disconnected; reconnecting");
      else {
        ready = false;
        status = "connecting";
        if (received === null && !history) unanchored = true;
        dirty = true;
        awaitReady(current);
      }
    });
  }

  function snapshot() {
    if (textDirty) {
      text = chunks.join("");
      textDirty = false;
    }
    return {
      paneId: pane, status, paused, suspended, history, recent, cursor: received,
      resumeCursor: receivedCursor?.token ?? null, terminal,
      text, discardedChars, gapNotifications: gaps, unanchored, error, capture, followFromSegment,
      captureIdentified: captureProvenance !== null,
      sequence: receivedSequence, sequenceDiscontinuities, sequenceRestarted, retainedPrefixMissing,
    };
  }

  function startSelection(selected, retainedHistory, target = null, recentHistory = false) {
    retire();
    pane = selected;
    capture = target;
    followFromSegment = null;
    captureProvenance = null;
    capturePending = target !== null;
    history = Boolean(retainedHistory);
    recent = recentHistory;
    paused = false;
    received = null;
    receivedCursor = null;
    receivedSequence = null;
    sequenceDiscontinuities = 0;
    sequenceRestarted = retainedPrefixMissing = false;
    chunks = [];
    chars = 0;
    text = "";
    textDirty = false;
    error = null;
    terminal = false;
    gaps = discardedChars = 0;
    unanchored = false;
    retryMs = 1000;
    // A historical read selected while the page is hidden waits for restore.
    status = !suspended ? "connecting" : capture === null ? "suspended" : "loading";
    dirty = true;
    open();
  }

  return {
    // A new explicit selection is a new identity boundary, even for the same
    // numeric pane. Navigation elsewhere must not silently call this method.
    select(paneId, { retainedHistory = false, recentHistory = false } = {}) {
      const selected = unsigned(paneId, U64_MAX);
      if (selected === null) throw new Error("invalid pane id");
      if (retainedHistory && recentHistory) throw new Error("choose one history window");
      startSelection(selected, retainedHistory, null, recentHistory);
    },
    // Search results identify a capture, not a current pane attachment. Keep
    // every identity as an exact decimal string; validate before retiring the
    // current view so malformed or obsolete UI actions cannot retarget it.
    selectCapture(hit) {
      const target = {
        paneId: unsigned(hit?.paneId, U64_MAX),
        segmentId: unsigned(hit?.segmentId, I64_MAX),
        capturedAt: unsigned(hit?.capturedAt, I64_MAX),
        contentLength: unsigned(hit?.contentLength, U64_MAX),
      };
      const boundary = cursorFor(hit?.captureCursor, target.paneId, true);
      if (Object.values(target).some((value) => value === null) || target.segmentId === "0"
          || boundary === null || BigInt(boundary.segment) !== BigInt(target.segmentId) - 1n) {
        throw new Error("invalid capture identity");
      }
      target.captureCursor = boundary.token;
      if (hit.paneUuid !== undefined) {
        if (typeof hit.paneUuid !== "string" || !/^[0-9a-f]{32}$/.test(hit.paneUuid)) throw new Error("invalid capture identity");
        target.paneUuid = hit.paneUuid;
      }
      startSelection(target.paneId, true, Object.freeze(target));
    },
    // An explicit new action turns a completed one-segment read into a
    // read-only stream. Keep its admitted epoch, segment and per-pane sequence
    // together: opening by numeric pane alone would skip intervening output.
    // A capture without a recorded UUID (output stored before capture
    // provenance, or a distributed remote pane) cannot be told apart from a
    // terminal that later reuses its pane ID, so it is not followed.
    followCapture() {
      if (status !== "historical" || capture === null || suspended || captureProvenance === null
          || receivedCursor?.segment !== capture.segmentId || receivedSequence === null) return false;
      followFromSegment = capture.segmentId;
      capture = null;
      open();
      return true;
    },
    pause() {
      if (pane === null || terminal || paused || capture !== null) return;
      retire();
      paused = true;
      status = "paused";
      dirty = true;
    },
    resume() {
      if (pane === null || terminal || !paused || capture !== null) return;
      paused = false;
      if (received === null && !history) unanchored = true;
      status = suspended ? "suspended" : "connecting";
      dirty = true;
      open();
    },
    suspend() {
      if (suspended) return;
      // A hidden page holds no connection. An unfinished historical read
      // stays pending ("loading"), and restore requests the same capture
      // again with a new deadline; nothing else can retry a pinned one.
      retire();
      suspended = true;
      if (pane !== null && capture === null && !terminal) status = paused ? "paused" : "suspended";
      dirty = true;
    },
    restore() {
      if (!suspended) return;
      suspended = false;
      if (pane !== null && capture === null && !terminal && !paused && received === null && !history) unanchored = true;
      if (pane !== null && capture === null && !terminal) status = paused ? "paused" : "connecting";
      dirty = true;
      open();
    },
    close() {
      retire();
      pane = null;
      capture = null;
      followFromSegment = null;
      captureProvenance = null;
      capturePending = false;
      received = null;
      receivedCursor = null;
      receivedSequence = null;
      sequenceDiscontinuities = 0;
      sequenceRestarted = retainedPrefixMissing = false;
      chunks = [];
      chars = 0;
      text = "";
      textDirty = false;
      error = null;
      terminal = false;
      paused = history = recent = unanchored = false;
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

function outputNotices(state, holdingSelection) {
  const notices = [];
  if (holdingSelection) notices.push("Output is paused to keep selected text stable. Clear the selection to show any newer received text; Resume continues the stream.");
  if (state.discardedChars) notices.push(state.capture !== null
    ? "This captured segment exceeds the view limit; only its bounded suffix is shown."
    : "Older text was discarded from this bounded view.");
  if (state.gapNotifications) notices.push(`${state.gapNotifications} capture-gap or bus-lag notifications; continuity is not guaranteed.`);
  if (state.sequenceDiscontinuities) notices.push(`${state.sequenceDiscontinuities} stored capture sequence discontinuities detected; output between retained segments may be missing. Load retained history to recover what remains.`);
  if (state.sequenceRestarted) notices.push("Capture sequence restarted or moved backwards; this view may span different capture histories.");
  if (state.retainedPrefixMissing) notices.push("Retained history starts after the beginning of capture; earlier output is unavailable in this view.");
  if (state.unanchored) notices.push("Reconnected before a received cursor existed; earlier output may be missing. Load retained history to recover what remains.");
  if (state.error) notices.push(state.error);
  return notices;
}

// Bind native, accessible controls to the same controller exercised by tests.
// The selected fleet row is sampled only on an explicit Open action. No pane
// title or capture content is interpreted as markup, a link, or a VT command.
export function mountPaneOutput({
  document: doc = globalThis.document,
  apiRoot,
  getSelectedPane,
  restoreFocus,
  onPin = () => {},
  onOpen = () => {},
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
  const recentButton = element("pane-output-recent");
  const closeButton = element("pane-output-close");
  const pinButton = element("pane-output-pin");
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
  // Whether the last render kept the displayed text for a selection.
  let heldSelection = false;
  const setText = (node, value) => {
    // Avoid repeating live-region announcements on every output frame.
    if (node.textContent !== value) node.textContent = value;
  };

  function render(force = false) {
    let state = output.takeUpdate() ?? (force ? output.snapshot() : null);
    if (!state) return;
    const opened = state.paneId !== null;
    const historical = state.capture !== null;
    const holdingSelection = opened && !historical
      && transcript.selectionStart !== transcript.selectionEnd;
    heldSelection = holdingSelection;
    if (holdingSelection) {
      // The displayed buffer can lag an admitted frame. Preserve the exact
      // selected text even if that frame already evicted its prefix; offsets
      // into the newer bounded buffer would identify different diagnostics.
      output.pause();
      state = output.takeUpdate() ?? state;
      follow.checked = false;
    }
    panel.hidden = !opened;
    workspace.dataset.outputOpen = String(opened);
    setText(title, historical ? `Pane ${state.paneId} — captured segment ${state.capture.segmentId}`
      : opened ? `Pane ${state.paneId} — captured output` : "Captured output");
    const captureStatus = state.status === "historical" ? "Historical capture loaded"
      : state.status === "loading" ? "Loading historical capture" : "Historical capture unavailable";
    const streamStart = state.followFromSegment !== null ? `from captured segment ${state.followFromSegment}`
      : state.history ? "following retained history" : state.recent
        ? `latest ${RECENT_SEGMENTS} retained segments and new output` : "new output since opening";
    setText(statusLine, historical ? `${captureStatus} · read-only · not following a current pane`
      : opened ? `${state.status} · read-only · ${streamStart}` : "");
    setText(historyButton, historical ? "Follow captured output from here" : "Load retained history");
    const unidentified = historical && state.status === "historical" && !state.captureIdentified;
    const historyTitle = unidentified
      ? "This capture has no recorded capture identity, so later output could come from another terminal that reused the pane ID."
      : historical
        ? "Read subsequent captures for this pane, starting after the displayed segment."
        : "Replace this view by replaying retained history from the beginning; large histories may take time.";
    if (historyButton.title !== historyTitle) historyButton.title = historyTitle;
    historyButton.disabled = historical && (state.status !== "historical" || state.suspended || unidentified);
    recentButton.hidden = historical || state.followFromSegment !== null;
    recentButton.disabled = recentButton.hidden || state.suspended || !opened;
    pinButton.disabled = !opened || state.suspended || (historical && state.status !== "historical");
    setText(pinButton, historical ? "Pin this capture" : "Pin pane output");
    follow.disabled = historical;
    pause.disabled = historical || state.terminal;
    setText(pause, state.paused ? "Resume" : "Pause");
    pause.setAttribute("aria-pressed", String(state.paused));
    const notices = outputNotices(state, holdingSelection);
    setText(warnings, notices.join(" "));
    warnings.hidden = notices.length === 0;
    if (!holdingSelection && transcript.value !== state.text) {
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

  function presentSelection(message, followLive) {
    onOpen();
    transcript.setSelectionRange(0, 0);
    paneInput.value = output.snapshot().paneId;
    hint.textContent = message;
    follow.checked = followLive;
    search.value = "";
    matchAt = -1;
    matchQuery = matchText = "";
    findStatus.textContent = "";
    render();
    if (!followLive) transcript.scrollTop = 0;
    transcript.focus();
  }

  function show(paneId, options) {
    try {
      output.select(paneId, options);
    } catch (_) {
      hint.textContent = "Choose a pane or enter a valid unsigned pane ID.";
      return;
    }
    presentSelection("Output stays on this pane until you explicitly open another.", true);
  }

  function openCapture(hit) {
    try {
      output.selectCapture(hit);
    } catch (_) {
      hint.textContent = "The search result has an invalid capture identity. Search again.";
      return false;
    }
    presentSelection("Showing one historical capture. Use Follow captured output from here to read subsequent captures.", false);
    return true;
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
    transcript.setSelectionRange(0, 0);
    output.close();
    matchAt = -1;
    matchQuery = matchText = "";
    search.value = "";
    findStatus.textContent = "";
    hint.textContent = "Captured output is read-only; no keys are sent to a pane.";
    render();
    if (restoreFocus) restoreFocus();
    else canvas.focus();
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
    const historical = output.snapshot().capture !== null;
    findStatus.textContent = found < 0 ? `Not found in retained text.${historical ? "" : " Stream paused."}`
      : historical ? "Match selected in this historical capture."
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
    if (output.snapshot().paused) {
      transcript.setSelectionRange(transcript.selectionEnd, transcript.selectionEnd);
      output.resume();
    } else output.pause();
    render();
  });
  historyButton.addEventListener("click", () => {
    const state = output.snapshot();
    if (state.capture !== null) {
      if (!output.followCapture()) return;
      transcript.setSelectionRange(transcript.selectionEnd, transcript.selectionEnd);
      follow.checked = true;
      hint.textContent = "Following captured output after the selected segment.";
      render();
    } else if (state.paneId !== null) show(state.paneId, { retainedHistory: true });
  });
  recentButton.addEventListener("click", () => {
    const state = output.snapshot();
    if (state.paneId !== null && !recentButton.disabled) show(state.paneId, { recentHistory: true });
  });
  closeButton.addEventListener("click", close);
  pinButton.addEventListener("click", () => {
    const state = output.snapshot();
    if (state.paneId === null || state.suspended || (state.capture !== null && state.status !== "historical")) return;
    onPin({ paneId: state.paneId, capture: state.capture });
  });
  follow.addEventListener("change", () => {
    if (follow.checked) {
      transcript.setSelectionRange(transcript.selectionEnd, transcript.selectionEnd);
      render(true);
      transcript.scrollTop = transcript.scrollHeight;
    }
  });
  // Native selection/copy owns the displayed text until the selection is
  // cleared. Also check it in render() in case a frame precedes these events.
  // Browsers fire select only when text becomes selected: a click or key
  // that clears the selection fires selectionchange, pointerup or keyup
  // instead. Re-render on those only while a selection exists or was held,
  // so caret moves during live following cost nothing.
  const selectionMayHaveChanged = () => {
    if (heldSelection || transcript.selectionStart !== transcript.selectionEnd) render(true);
  };
  for (const type of ["select", "selectionchange", "pointerup", "keyup"]) {
    transcript.addEventListener(type, selectionMayHaveChanged);
  }
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
    render, openSelected, openCapture, close,
    suspend() { output.suspend(); render(); },
    restore() { output.restore(); render(); },
  };
}

// A small, session-owned watchboard for comparing captured output. Each tile
// uses the same epoch-fenced SSE owner as the inspector. Pins are deliberately
// memory-only: a numeric capture pane ID is not a durable live-pane identity.
// Three pins plus the inspector and fleet feed own at most five SSE connections
// in this tab, leaving an HTTP/1 origin connection for session checks and
// ordinary reads only while no other tab of the origin holds connections. Every
// tab shares the browser's six per-origin connections and the session's eight
// server streams.
// Each pin retains at most 128 Ki UTF-16 units; no preferences or data persist.
export function mountPinnedOutputs({
  document: doc = globalThis.document,
  apiRoot,
  createOutput = () => createPaneOutput({ apiRoot, maxChars: 128 * 1024, maxChunks: 1024 }),
} = {}) {
  const workspace = doc.getElementById("workspace");
  const board = doc.getElementById("pinned-output");
  const title = doc.getElementById("pinned-output-title");
  const status = doc.getElementById("pinned-output-status");
  const grid = doc.getElementById("pinned-output-grid");
  const clear = doc.getElementById("pinned-output-clear");
  const pinButton = doc.getElementById("pane-output-pin");
  if ([workspace, board, title, status, grid, clear, pinButton].some((node) => !node)) {
    throw new Error("missing pinned output controls");
  }
  const cards = new Map();
  const limit = 3;
  let suspended = true;
  let disposed = false;
  let focused = null;
  let nextId = 0;
  const text = (node, value) => { if (node.textContent !== value) node.textContent = value; };
  const make = (tag, className, value) => {
    const node = doc.createElement(tag);
    if (className) node.className = className;
    if (value !== undefined) node.textContent = value;
    if (tag === "button") node.type = "button";
    return node;
  };
  const restoreControlFocus = () => {
    // The inspector may have been closed while its pins keep running. Never
    // return focus to a control inside that now-hidden panel.
    const target = doc.getElementById("pane-output").hidden
      ? doc.getElementById("pane-output-id") : pinButton;
    target.focus();
  };
  function layout() {
    board.hidden = cards.size === 0;
    workspace.dataset.pinsOpen = String(cards.size > 0);
    workspace.dataset.pinnedFocus = String(focused !== null);
    text(title, `Pinned output (${cards.size}/${limit})`);
    clear.disabled = cards.size === 0 || suspended || disposed;
    for (const [key, card] of cards) {
      card.panel.hidden = focused !== null && focused !== key;
      text(card.focus, focused === key ? "Show all panes" : "Focus pane");
      card.focus.setAttribute("aria-pressed", String(focused === key));
    }
  }
  function remove(key) {
    const card = cards.get(key);
    if (!card) return;
    card.output.close();
    card.transcript.value = "";
    card.panel.remove();
    cards.delete(key);
    if (focused === key) focused = null;
    text(status, cards.size ? "Removed the pinned view. Other captures keep their own position." : "");
    layout();
    if (!suspended && !disposed) {
      const next = cards.values().next().value;
      if (next) next.focus.focus();
      else restoreControlFocus();
    }
  }
  function clearAll() {
    for (const card of cards.values()) {
      card.output.close();
      card.transcript.value = "";
      card.panel.remove();
    }
    cards.clear();
    focused = null;
    text(status, "");
    layout();
  }
  clear.addEventListener("click", () => {
    if (suspended || disposed) return;
    clearAll();
    restoreControlFocus();
  });

  function pin({ paneId, capture = null } = {}) {
    if (suspended || disposed) return false;
    const pane = unsigned(paneId, U64_MAX);
    if (pane === null || (capture !== null && capture?.paneId !== pane)) {
      text(status, "Choose a valid captured pane before pinning.");
      return false;
    }
    const key = capture === null ? `pane:${pane}` : `capture:${capture.captureCursor}`;
    const existing = cards.get(key);
    if (existing) {
      focused = null;
      // A capture that timed out or failed has nothing to preserve, and its
      // tile has no retry control, so pinning it again reads it again.
      const retry = capture !== null && existing.output.snapshot().status === "unavailable";
      if (retry) {
        try {
          existing.output.selectCapture(capture);
        } catch (_) {
          text(status, "The capture could not be pinned. Open the pane or search for that capture again.");
          return false;
        }
        existing.render(true);
      }
      layout();
      text(status, retry ? "Reading that pinned capture again."
        : "That view is already pinned. Its capture history and position were preserved.");
      existing.transcript.focus();
      return true;
    }
    if (cards.size >= limit) {
      text(status, "Three views are already pinned. Remove one before adding another.");
      clear.focus();
      return false;
    }
    const output = createOutput();
    try {
      if (capture === null) output.select(pane);
      else output.selectCapture(capture);
    } catch (_) {
      output.close();
      text(status, "The capture could not be pinned. Open the pane or search for that capture again.");
      return false;
    }
    const panel = make("section", "pinned-output-card");
    const controls = make("div", "output-toolbar");
    const heading = make("h3", "", capture === null ? `Pane ${pane}` : `Pane ${pane} · segment ${capture.segmentId}`);
    heading.id = `pinned-output-heading-${++nextId}`;
    panel.setAttribute("aria-labelledby", heading.id);
    const focus = make("button", "", "Focus pane");
    const removeButton = make("button", "", "Remove");
    removeButton.setAttribute("aria-label", `Remove pinned output for pane ${pane}`);
    controls.append(heading, focus, removeButton);
    const actions = make("div", "output-toolbar");
    const pause = make("button", "", "Pause");
    const history = make("button", "", "Load retained history");
    const recentOutput = make("button", "", "Load recent output");
    recentOutput.title = `Show the latest ${RECENT_SEGMENTS} retained segments for this pane ID, then follow new output.`;
    const followLabel = make("label", "", "");
    const follow = make("input");
    follow.type = "checkbox";
    follow.checked = capture === null;
    followLabel.append(follow, make("span", "", " Follow output"));
    actions.append(pause, recentOutput, history, followLabel);
    const stateLine = make("p", "pinned-output-state");
    stateLine.setAttribute("role", "status");
    const warnings = make("p", "pinned-output-warnings");
    warnings.setAttribute("role", "status");
    const transcript = make("textarea", "pinned-output-text");
    transcript.readOnly = true;
    transcript.spellcheck = false;
    transcript.wrap = "off";
    transcript.setAttribute("aria-label", capture === null
      ? `Pinned captured output for pane ${pane}, read-only`
      : `Pinned captured segment ${capture.segmentId} for pane ${pane}, read-only`);
    panel.append(controls, stateLine, actions, warnings, transcript);
    let heldSelection = false;
    const current = () => !disposed && cards.get(key)?.output === output;
    function render(force = false) {
      if (!current()) return;
      let state = output.takeUpdate() ?? (force ? output.snapshot() : null);
      if (!state) return;
      const historical = state.capture !== null;
      const holdingSelection = !historical && transcript.selectionStart !== transcript.selectionEnd;
      heldSelection = holdingSelection;
      if (holdingSelection) {
        output.pause();
        state = output.takeUpdate() ?? state;
        follow.checked = false;
      }
      text(stateLine, historical ? `${state.status} · one historical capture · read-only`
        : `${state.status} · read-only · ${state.history ? "following retained history" : state.recent
          ? `latest ${RECENT_SEGMENTS} retained segments and new output` : "new output since pinning"}`);
      text(pause, state.paused ? "Resume" : "Pause");
      pause.setAttribute("aria-pressed", String(state.paused));
      pause.disabled = historical || state.terminal || suspended;
      history.disabled = historical || suspended;
      history.hidden = historical;
      recentOutput.disabled = historical || suspended;
      recentOutput.hidden = historical;
      follow.disabled = historical || suspended;
      focus.disabled = removeButton.disabled = suspended;
      const notices = outputNotices(state, holdingSelection);
      text(warnings, notices.join(" "));
      warnings.hidden = notices.length === 0;
      if (!holdingSelection && transcript.value !== state.text) {
        const scrollTop = transcript.scrollTop;
        transcript.value = state.text;
        if (follow.checked) transcript.scrollTop = transcript.scrollHeight;
        else transcript.scrollTop = scrollTop;
      }
    }
    cards.set(key, { output, panel, transcript, focus, render });
    grid.append(panel);
    focused = null;
    focus.addEventListener("click", () => {
      if (!current() || suspended) return;
      focused = focused === key ? null : key;
      layout();
      focus.focus();
    });
    removeButton.addEventListener("click", () => { if (current() && !suspended) remove(key); });
    pause.addEventListener("click", () => {
      if (!current() || suspended) return;
      if (output.snapshot().paused) {
        transcript.setSelectionRange(transcript.selectionEnd, transcript.selectionEnd);
        output.resume();
      } else output.pause();
      render();
    });
    function loadHistory(options) {
      if (!current() || suspended || capture !== null) return;
      transcript.setSelectionRange(0, 0);
      output.select(pane, options);
      follow.checked = true;
      render();
    }
    history.addEventListener("click", () => loadHistory({ retainedHistory: true }));
    recentOutput.addEventListener("click", () => loadHistory({ recentHistory: true }));
    follow.addEventListener("change", () => {
      if (!current() || suspended || !follow.checked) return;
      transcript.setSelectionRange(transcript.selectionEnd, transcript.selectionEnd);
      render(true);
      transcript.scrollTop = transcript.scrollHeight;
    });
    const selectionChanged = () => {
      if (current() && (heldSelection || transcript.selectionStart !== transcript.selectionEnd)) render(true);
    };
    for (const type of ["select", "selectionchange", "pointerup", "keyup"]) {
      transcript.addEventListener(type, selectionChanged);
    }
    transcript.addEventListener("scroll", () => {
      if (transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight > 8) follow.checked = false;
    });
    panel.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && focused === key && current()) {
        event.preventDefault();
        focused = null;
        layout();
        focus.focus();
      }
    });
    layout();
    render();
    text(status, capture === null
      ? "Pinned a separate view of new captured output. Load retained history on a tile to include earlier capture."
      : "Pinned the exact historical capture. This tile does not follow a current pane.");
    transcript.focus();
    return true;
  }
  layout();
  return {
    pin,
    showAll() { if (!disposed) { focused = null; layout(); } },
    render() { if (!suspended && !disposed) for (const card of cards.values()) card.render(); },
    suspend() {
      if (suspended || disposed) return;
      suspended = true;
      for (const card of cards.values()) { card.output.suspend(); card.render(true); }
      layout();
    },
    restore() {
      if (!suspended || disposed) return;
      suspended = false;
      for (const card of cards.values()) { card.output.restore(); card.render(true); }
      layout();
    },
    dispose() {
      if (disposed) return;
      disposed = suspended = true;
      clearAll();
    },
  };
}
