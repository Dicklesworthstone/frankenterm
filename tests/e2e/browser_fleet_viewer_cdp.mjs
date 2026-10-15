// Real-browser driver for test_browser_fleet_viewer.sh, over raw CDP (no
// Playwright). Launches an isolated headless Chrome, opens the fleet viewer
// served by `ft web --ui-dir`, and checks what a person would see.
//
// Usage: node browser_fleet_viewer_cdp.mjs <config.json>
// config: { url, chrome, profileDir, debugPort, screenshot, resultPath,
//           producerExitFile, expectDetection }
// The producer runs beside this driver; it writes its exit status to
// producerExitFile when done. Exit 0 only when every check passed; results
// JSON is written either way.
import { spawn } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";

const config = JSON.parse(readFileSync(process.argv[2], "utf8"));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const results = { checks: [], page_errors: [], console: [] };
function check(name, ok, detail = "") {
  results.checks.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` — ${detail}` : ""}`);
}

const chrome = spawn(config.chrome, [
  "--headless=new",
  `--remote-debugging-port=${config.debugPort}`,
  `--user-data-dir=${config.profileDir}`,
  "--no-first-run",
  "--no-default-browser-check",
  "--enable-unsafe-webgpu",
  "--window-size=1280,800",
  "about:blank",
], { stdio: "ignore" });

let socket;
try {
  let target;
  for (let i = 0; i < 300 && !target; i++) {
    await sleep(100);
    try {
      const list = await (await fetch(`http://127.0.0.1:${config.debugPort}/json/list`)).json();
      target = list.find((t) => t.type === "page");
    } catch { /* Chrome is still starting */ }
  }
  if (!target) throw new Error("Chrome exposed no CDP page target within 30 s");

  socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
  let nextId = 1;
  const pending = new Map();
  socket.onmessage = (message) => {
    const data = JSON.parse(message.data);
    if (data.id && pending.has(data.id)) {
      const { resolve, reject } = pending.get(data.id);
      pending.delete(data.id);
      if (data.error) reject(new Error(data.error.message)); else resolve(data.result);
    } else if (data.method === "Runtime.exceptionThrown") {
      const details = data.params.exceptionDetails;
      results.page_errors.push(details?.exception?.description ?? details?.text ?? "exception");
    } else if (data.method === "Runtime.consoleAPICalled") {
      const text = data.params.args.map((arg) => arg.value ?? arg.description ?? "").join(" ");
      results.console.push(`${data.params.type}: ${text}`);
      if (data.params.type === "error") results.page_errors.push(text);
    } else if (data.method === "Log.entryAdded" && data.params.entry.level === "error") {
      results.page_errors.push(`${data.params.entry.source}: ${data.params.entry.text} ${data.params.entry.url ?? ""}`);
    }
  };
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async (expression) =>
    (await send("Runtime.evaluate", { expression, returnByValue: true })).result.value;
  const screen = async () =>
    (await evaluate("document.getElementById('screen-text')?.textContent ?? ''")) ?? "";
  async function waitForScreen(predicate, timeoutMs) {
    const started = Date.now();
    let text = await screen();
    while (!predicate(text) && Date.now() - started < timeoutMs) {
      await sleep(100);
      text = await screen();
    }
    return { text, ms: Date.now() - started };
  }
  async function pressKey(key, code, keyCode) {
    const event = { key, code, windowsVirtualKeyCode: keyCode, nativeVirtualKeyCode: keyCode };
    await send("Input.dispatchKeyEvent", { type: "keyDown", ...event, text: key.length === 1 ? key : undefined });
    await send("Input.dispatchKeyEvent", { type: "keyUp", ...event });
  }

  await send("Runtime.enable");
  await send("Log.enable");
  await send("Page.enable");
  await send("Page.navigate", { url: config.url });

  let ready = null;
  for (let i = 0; i < 300 && !ready; i++) {
    ready = await evaluate("document.documentElement.dataset.viewerReady ?? null");
    if (!ready) await sleep(100);
  }
  const startError = await evaluate(
    "document.getElementById('error')?.hidden === false ? document.getElementById('error').textContent : ''",
  );
  check("viewer starts from ft web /ui/", ready === "1", ready === "1" ? "" : `state=${ready} ${startError}`);

  let { text } = await waitForScreen((t) => t.includes("live ·"), 10_000);
  check("labelled read-only", text.includes("fleet viewer (read-only)"));
  check("event stream is live", text.includes("live ·"), text.split("\n")[0]?.trim());

  // The detection comes from the real capture -> detect pipeline running
  // beside this driver. Watch the Events view while it works.
  await pressKey("Tab", "Tab", 9);
  ({ text } = await waitForScreen((t) => t.includes(config.expectDetection), 180_000));
  check(`pipeline detection ${config.expectDetection} shown`, text.includes(config.expectDetection));

  const producerWaitStarted = Date.now();
  while (!existsSync(config.producerExitFile) && Date.now() - producerWaitStarted < 180_000) {
    await sleep(200);
  }
  const producerExit = existsSync(config.producerExitFile)
    ? readFileSync(config.producerExitFile, "utf8").trim()
    : "timeout";
  check("production-path producer passed", producerExit === "0", `exit=${producerExit}`);

  // Back to panes; ask for a refresh so the producer's pane is listed.
  await pressKey("1", "Digit1", 49);
  await pressKey("r", "KeyR", 82);
  const panes = await waitForScreen((t) => /\s([1-9]\d*) panes/.test(t.split("\n")[0] ?? ""), 10_000);
  const paneCount = Number((panes.text.split("\n")[0] ?? "").match(/\s(\d+) panes/)?.[1] ?? 0);
  check("captured pane listed", paneCount >= 1, `${paneCount} pane(s)`);

  const shot = await send("Page.captureScreenshot", { format: "png" });
  writeFileSync(config.screenshot, Buffer.from(shot.data, "base64"));
  results.final_screen = panes.text;
  check("no page errors", results.page_errors.length === 0, results.page_errors.slice(0, 3).join(" | "));
} catch (error) {
  check("driver", false, String(error?.stack ?? error));
} finally {
  try { socket?.close(); } catch { /* already closed */ }
  chrome.kill("SIGTERM");
}

writeFileSync(config.resultPath, JSON.stringify(results, null, 2));
const failed = results.checks.filter((c) => !c.ok).length;
console.log(`${results.checks.length - failed}/${results.checks.length} browser checks passed`);
process.exit(failed === 0 ? 0 : 1);
