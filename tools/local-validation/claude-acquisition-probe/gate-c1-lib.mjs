export const expectedExtension = Object.freeze({ name: "QuotaBar local Claude acquisition probe", version: "0.1.0" });
export const expectedRequests = Object.freeze([Object.freeze({ category: "synthetic", method: "GET", path: "/" })]);

export function assertSmokeReport(report) {
  const fail = (code) => { throw new Error(`smoke-assertion:${code}`); };
  if (!report.extension?.id || report.extension.name !== expectedExtension.name || report.extension.version !== expectedExtension.version || report.extension.enabled !== true) fail("extension-identity");
  if (report.synthetic?.origin !== "exact" || report.synthetic?.marker !== true) fail("synthetic-origin-marker");
  if (report.buildIdExpected !== "p4b-1e-r" || report.sessionView?.buildId !== "p4b-1e-r") fail("build-id");
  if (report.sessionView?.diagnostic !== "bridge_received_main_ready") fail("final-handshake");
  if (report.sessionView?.bootstrap?.bridgeLoaded !== true || report.sessionView?.bootstrap?.observerReady !== true) fail("bootstrap-confirmation");
  if (!Array.isArray(report.postOrder) || report.postOrder.join(",") !== "observer_installed,main_ready,observer_installed,main_ready") fail("main-readiness-order");
  if (report.exceptions?.length !== 0) fail("startup-exception");
  if (report.networkObserved !== true || report.targetObserved !== true) fail("observation-incomplete");
  if (JSON.stringify(report.requests) !== JSON.stringify(expectedRequests)) fail("loopback-request-set");
  if (report.externalRequest !== false || report.unexpectedRequest !== false || report.unexpectedTarget !== false) fail("unexpected-target-or-network");
  return true;
}

export function smokeExitCode(report) { try { assertSmokeReport(report); return 0; } catch { return 1; } }

export function raceStartup(operation, earlyExit) { return Promise.race([operation, earlyExit]); }

export function withDeadline(promise, timeoutMs, code, onTimeout = () => undefined) {
  return new Promise((resolve, reject) => {
    let settled = false;
    const finish = (callback, value) => { if (!settled) { settled = true; clearTimeout(timer); callback(value); } };
    const timer = setTimeout(() => { try { onTimeout(); } finally { finish(reject, new Error(code)); } }, timeoutMs);
    Promise.resolve(promise).then((value) => finish(resolve, value), (error) => finish(reject, error));
  });
}

export function openWebSocket(WebSocketConstructor, url, timeoutMs, signal) {
  return new Promise((resolve, reject) => {
    let settled = false; let socket;
    const finish = (callback, value) => { if (!settled) { settled = true; clearTimeout(timer); signal?.removeEventListener("abort", abort); callback(value); } };
    const abort = () => { try { socket?.close(); } catch {} finish(reject, new Error("websocket-open-aborted")); };
    socket = new WebSocketConstructor(url);
    const timer = setTimeout(() => { try { socket.close(); } catch {} finish(reject, new Error("websocket-open-timeout")); }, timeoutMs);
    signal?.addEventListener("abort", abort, { once: true });
    socket.onopen = () => finish(resolve, socket);
    socket.onerror = () => finish(reject, new Error("websocket-open-error"));
    socket.onclose = () => finish(reject, new Error("websocket-close-before-open"));
  });
}

export async function terminateChild(child, sleep, termGraceMs, killGraceMs) {
  if (child.exitCode !== null || child.signalCode !== null) return "already-exited";
  const waitForExit = () => new Promise((resolve) => child.once("exit", resolve));
  let exited = waitForExit();
  let sent;
  try { sent = child.kill("SIGTERM"); } catch { throw new Error("child-sigterm-throw"); }
  if (sent !== true) throw new Error("child-sigterm-failed");
  if (await withDeadline(exited, termGraceMs, "child-term-timeout").then(() => true, () => false)) return "sigterm";
  exited = waitForExit();
  try { sent = child.kill("SIGKILL"); } catch { throw new Error("child-sigkill-throw"); }
  if (sent !== true) throw new Error("child-sigkill-failed");
  if (await withDeadline(exited, killGraceMs, "child-kill-timeout").then(() => true, () => false)) return "sigkill";
  throw new Error("child-kill-timeout");
}
