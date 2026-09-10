import { spawn, execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:https";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { assertSmokeReport, expectedExtension, expectedRequests, openWebSocket, terminateChild } from "./gate-c1-lib.mjs";

const execFileAsync = promisify(execFile);
const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms));
const chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const probe = dirname(fileURLToPath(import.meta.url));
const overallDeadlineMs = 20_000;
const overallDeadlineAt = Date.now() + overallDeadlineMs;
let root; let child; let server; let rootSocket; let lastReport; const attached = []; const createdTargets = [];

function fail(code) { throw new Error(`harness:${code}`); }
function withDeadline(promise, code) { const remaining = overallDeadlineAt - Date.now(); if (remaining <= 0) return Promise.reject(new Error(`harness:${code}`)); return Promise.race([promise, sleep(remaining).then(() => { throw new Error(`harness:${code}`); })]); }
function sanitizedException(details) {
  const source = ["isolated-entry.js", "main-entry.js", "probe-core.js"].find((name) => String(details.url || "").endsWith(`/${name}`));
  if (!source) return null;
  return { source, line: Number.isInteger(details.lineNumber) ? details.lineNumber + 1 : null, class: String(details.exception?.description || details.text || "").includes("TypeError") ? "TypeError" : "ExtensionError" };
}
async function waitForPort(profile) {
  for (let attempt = 0; attempt < 40; attempt += 1) { try { return (await readFile(join(profile, "DevToolsActivePort"), "utf8")).trim().split("\n"); } catch { await sleep(100); } }
  fail("devtools-active-port-unavailable");
}
function createCdp(socket, onEvent) {
  let nextId = 0; const pending = new Map();
  socket.addEventListener("message", (event) => { const message = JSON.parse(event.data); if (message.id && pending.has(message.id)) { const done = pending.get(message.id); pending.delete(message.id); done(message); } else onEvent(message); });
  return (method, params = {}, sessionId) => new Promise((resolveCall, rejectCall) => { const id = ++nextId; const timer = setTimeout(() => { pending.delete(id); rejectCall(new Error(`harness:cdp-timeout:${method}`)); }, 4_000); pending.set(id, (message) => { clearTimeout(timer); resolveCall(message); }); socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) })); });
}
async function startServer(cert, key) {
  const requests = [];
  server = createServer({ cert, key }, (request, response) => { requests.push({ method: request.method, path: request.url }); response.writeHead(200, { "content-type": "text/html" }); response.end("<!doctype html><head><link rel=\"icon\" href=\"data:,\"></head><body data-qb-marker=\"quotabar-synthetic\"></body>"); });
  await new Promise((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
  return { port: server.address().port, requests };
}
async function evaluate(call, sessionId, expression, contextId) { const result = await call("Runtime.evaluate", { expression, contextId, returnByValue: true, awaitPromise: true }, sessionId); return result.result?.result?.value; }
async function closeSession(call, sessionId) { if (sessionId) await call("Target.detachFromTarget", { sessionId }).catch(() => undefined); }

try {
  root = await mkdtemp(join(tmpdir(), "quotabar-c1-"));
  const profile = join(root, "profile"), certPath = join(root, "cert.pem"), keyPath = join(root, "key.pem");
  await withDeadline(execFileAsync("openssl", ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", keyPath, "-out", certPath, "-subj", "/CN=claude.ai", "-addext", "subjectAltName=DNS:claude.ai", "-days", "1"]), "certificate-timeout");
  const synthetic = await startServer(await readFile(certPath), await readFile(keyPath));
  const args = ["--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--disable-component-update", "--disable-sync", "--no-pings", "--ignore-certificate-errors", "--host-resolver-rules=MAP * 127.0.0.1, EXCLUDE localhost", "about:blank"];
  child = spawn(chrome, args, { stdio: ["ignore", "ignore", "pipe"] });
  const earlyExit = new Promise((_, reject) => { child.once("error", () => reject(new Error("harness:chrome-spawn-error"))); child.once("exit", () => reject(new Error("harness:chrome-early-exit"))); });
  const [port, wsPath] = await withDeadline(waitForPort(profile), "devtools-timeout");
  rootSocket = await withDeadline(openWebSocket(WebSocket, `ws://127.0.0.1:${port}${wsPath}`, 2_000), "websocket-timeout");
  const contexts = []; const frames = []; const exceptions = []; const postOrder = [];
  const call = createCdp(rootSocket, (event) => { if (event.method === "Runtime.executionContextCreated") contexts.push(event.params.context); if (event.method === "Page.frameNavigated") frames.push(event.params.frame); if (event.method === "Runtime.exceptionThrown") { const value = sanitizedException(event.params.exceptionDetails); if (value) exceptions.push(value); } });
  await Promise.race([withDeadline((async () => {
    const load = await call("Extensions.loadUnpacked", { path: probe }); if (!load.result?.id) fail("extension-load");
    const extension = (await call("Extensions.getExtensions")).result?.extensions?.find((item) => item.id === load.result.id);
    if (!extension || extension.name !== expectedExtension.name || extension.version !== expectedExtension.version || extension.enabled !== true) fail("extension-identity");
    const created = await call("Target.createTarget", { url: "about:blank" }); createdTargets.push(created.result.targetId); const page = await call("Target.attachToTarget", { targetId: created.result.targetId, flatten: true }); attached.push(page.result.sessionId);
    for (const method of ["Runtime.enable", "Page.enable", "Log.enable"]) await call(method, {}, page.result.sessionId);
    await call("Page.addScriptToEvaluateOnNewDocument", { source: "(() => { const post = window.postMessage.bind(window); const allowed = new Set(['quotabar-local-probe-stage','quotabar-local-probe-main-ready','quotabar-local-probe-ready-request']); window.postMessage = (data, origin) => { if (allowed.has(data?.type)) { const value = data.type === 'quotabar-local-probe-stage' ? data.stage : data.type === 'quotabar-local-probe-main-ready' ? 'main_ready' : 'ready_request'; const prior = globalThis.__qbOrder || []; if (prior.length < 4) globalThis.__qbOrder = [...prior, value]; } return post(data, origin); }; })();" }, page.result.sessionId);
    const contextStart = contexts.length; const navigation = await call("Page.navigate", { url: `https://claude.ai:${synthetic.port}/` }, page.result.sessionId); if (navigation.error) fail("synthetic-navigation");
    const end = overallDeadlineAt; let frameId;
    while (Date.now() < end) { frameId = frames.find((frame) => frame.url === `https://claude.ai:${synthetic.port}/`)?.id; if (frameId && contexts.slice(contextStart).filter((item) => item.auxData?.frameId === frameId).length >= 2) break; await sleep(50); }
    if (!frameId) fail("synthetic-main-frame");
    const main = contexts.slice(contextStart).find((item) => item.auxData?.frameId === frameId && item.auxData?.isDefault === true); const isolated = contexts.slice(contextStart).find((item) => item.auxData?.frameId === frameId && item.auxData?.type === "isolated"); if (!main || !isolated) fail("worlds-unavailable");
    const [origin, marker, order] = await Promise.all([evaluate(call, page.result.sessionId, "location.origin", main.id), evaluate(call, page.result.sessionId, "document.body.dataset.qbMarker", main.id), evaluate(call, page.result.sessionId, "JSON.stringify(globalThis.__qbOrder || [])", main.id)]); postOrder.push(...JSON.parse(order || "[]"));
    const targets = (await call("Target.getTargets")).result?.targetInfos || []; const worker = targets.find((target) => target.type === "service_worker" && target.url.startsWith(`chrome-extension://${extension.id}/`)); if (!worker) fail("extension-worker-unavailable"); const workerSession = await call("Target.attachToTarget", { targetId: worker.targetId, flatten: true }); attached.push(workerSession.result.sessionId); await call("Runtime.enable", {}, workerSession.result.sessionId);
    const readView = async () => JSON.parse(await evaluate(call, workerSession.result.sessionId, "chrome.storage.session.get('quotabarProbeState').then(({ quotabarProbeState: state }) => JSON.stringify({ buildId: globalThis.QuotaBarProbeCore.BUILD_ID, diagnostic: state?.diagnostics?.[state?.slot], bootstrap: state?.bootstrap?.[state?.slot] }))")); let view = await readView(); const readinessEnd = Date.now() + 1_000; while (view.diagnostic !== "bridge_received_main_ready" && Date.now() < readinessEnd) { await sleep(50); view = await readView(); }
    const targetSummary = [...new Set(targets.map((target) => target.url === "about:blank" ? `${target.type}:blank` : target.url === `https://claude.ai:${synthetic.port}/` ? `${target.type}:synthetic` : target.url.startsWith(`chrome-extension://${extension.id}/`) ? `${target.type}:expected-extension` : target.url.startsWith("chrome-extension://") ? `${target.type}:other-extension` : target.url.startsWith("chrome://") || target.url.startsWith("devtools://") ? `${target.type}:chrome-internal` : `${target.type}:other`))].sort(); const unexpectedTarget = targetSummary.includes("page:other");
    const report = { extension: { id: extension.id, name: extension.name, version: extension.version, enabled: extension.enabled }, synthetic: { origin: origin === `https://claude.ai:${synthetic.port}` ? "exact" : "mismatch", marker: marker === "quotabar-synthetic" }, buildIdExpected: "p4b-1e-r", sessionView: view, postOrder, exceptions, requests: synthetic.requests, targetSummary, unexpectedTarget, externalRequest: false };
    lastReport = report; assertSmokeReport(report);
  })(), "overall-deadline"), earlyExit]);
} catch (error) { process.stdout.write(`${JSON.stringify({ gate: "R1", verdict: "blocked", failure: error instanceof Error ? error.message : "unknown", postOrder: lastReport?.postOrder || [], targetSummary: lastReport?.targetSummary || [] })}\n`); process.exitCode = 1; }
finally {
  let cleanup = "complete";
  try { if (rootSocket) { const call = createCdp(rootSocket, () => undefined); for (const targetId of createdTargets.reverse()) await call("Target.closeTarget", { targetId }).catch(() => undefined); for (const sessionId of attached.reverse()) await closeSession(call, sessionId); rootSocket.close(); } } catch { cleanup = "partial"; }
  try { if (child) await terminateChild(child, sleep, 500); } catch { cleanup = "partial"; }
  try { if (server) await new Promise((resolveClose) => server.close(resolveClose)); } catch { cleanup = "partial"; }
  try { if (root?.startsWith(`${resolve(tmpdir())}/quotabar-c1-`)) await rm(root, { recursive: true, force: true, maxRetries: 3, retryDelay: 200 }); else cleanup = "partial"; } catch { cleanup = "partial"; }
  if (cleanup !== "complete" && !process.exitCode) { process.stdout.write(`${JSON.stringify({ gate: "R1", verdict: "blocked", failure: "harness:cleanup-partial" })}\n`); process.exitCode = 1; }
  if (!process.exitCode && lastReport) process.stdout.write(`${JSON.stringify({ gate: "R1", ...lastReport, cleanup })}\n`);
}
