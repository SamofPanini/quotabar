export const expectedExtension = Object.freeze({ name: "QuotaBar local Claude acquisition probe", version: "0.1.0" });
export const expectedRequests = Object.freeze([Object.freeze({ method: "GET", path: "/" })]);

export function assertSmokeReport(report) {
  const fail = (code) => { throw new Error(`smoke-assertion:${code}`); };
  if (!report.extension?.id || report.extension.name !== expectedExtension.name || report.extension.version !== expectedExtension.version || report.extension.enabled !== true) fail("extension-identity");
  if (report.synthetic?.origin !== "exact" || report.synthetic?.marker !== true) fail("synthetic-origin-marker");
  if (report.buildIdExpected !== "p4b-1e-r" || report.sessionView?.buildId !== "p4b-1e-r") fail("build-id");
  if (report.sessionView?.diagnostic !== "bridge_received_main_ready") fail("final-handshake");
  if (report.sessionView?.bootstrap?.bridgeLoaded !== true || report.sessionView?.bootstrap?.observerReady !== true) fail("bootstrap-confirmation");
  if (!Array.isArray(report.postOrder) || report.postOrder.join(",") !== "observer_installed,main_ready,observer_installed,main_ready") fail("main-readiness-order");
  if (report.exceptions?.length !== 0) fail("startup-exception");
  if (JSON.stringify(report.requests) !== JSON.stringify(expectedRequests)) fail("loopback-request-set");
  if (report.unexpectedTarget !== false || report.externalRequest !== false) fail("unexpected-target-or-network");
  return true;
}

export function smokeExitCode(report) {
  try { assertSmokeReport(report); return 0; } catch { return 1; }
}

export function openWebSocket(WebSocketConstructor, url, timeoutMs) {
  return new Promise((resolve, reject) => {
    let settled = false;
    const finish = (callback, value) => { if (!settled) { settled = true; clearTimeout(timer); callback(value); } };
    const socket = new WebSocketConstructor(url);
    const timer = setTimeout(() => { try { socket.close(); } catch {} finish(reject, new Error("websocket-open-timeout")); }, timeoutMs);
    socket.onopen = () => finish(resolve, socket);
    socket.onerror = () => finish(reject, new Error("websocket-open-error"));
    socket.onclose = () => finish(reject, new Error("websocket-close-before-open"));
  });
}

export async function terminateChild(child, sleep, graceMs) {
  if (child.exitCode !== null || child.signalCode !== null) return "already-exited";
  const waitForExit = () => new Promise((resolve) => child.once("exit", resolve));
  const exited = waitForExit();
  child.kill("SIGTERM");
  if (await Promise.race([exited.then(() => true), sleep(graceMs).then(() => false)])) return "sigterm";
  child.kill("SIGKILL");
  await exited;
  return "sigkill";
}
