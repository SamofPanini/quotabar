import { spawn, execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:https";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { assertSmokeReport, coordinateCleanup, createCdpClient, expectedExtension, normalizeAbort, openWebSocket, raceStartup, terminateChild, withDeadline } from "./gate-c1-lib.mjs";

const execFileAsync = promisify(execFile);
const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms));
const chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const probe = dirname(fileURLToPath(import.meta.url));
const overallDeadlineMs = 20_000;
const overallDeadlineAt = Date.now() + overallDeadlineMs;
let root; let child; let server; let rootSocket; let client; let mainRun; let lastReport; let primaryFailure; let finalized = false; const attached = []; const createdTargets = [];
const aborter = new AbortController();

function fail(code) { throw new Error(`harness:${code}`); }
function freeze() { if (finalized) return; finalized = true; aborter.abort(); client?.close(); }
function withinOverall(promise, code) { const remaining = overallDeadlineAt - Date.now(); if (remaining <= 0) { freeze(); return Promise.reject(new Error(`harness:${code}`)); } return withDeadline(promise, remaining, `harness:${code}`, freeze); }
function sanitizedException(details) {
  const source = ["isolated-entry.js", "main-entry.js", "probe-core.js"].find((name) => String(details.url || "").endsWith(`/${name}`));
  if (!source) return null;
  return { source, line: Number.isInteger(details.lineNumber) ? details.lineNumber + 1 : null, class: String(details.exception?.description || details.text || "").includes("TypeError") ? "TypeError" : "ExtensionError" };
}
function fixedMethod(value) { return ["GET", "POST"].includes(value) ? value : "OTHER"; }
function classifyUrl(value, syntheticOrigin, extensionId) {
  if (typeof value !== "string") return "missing";
  if (value.startsWith("data:") || value.startsWith("chrome://") || value.startsWith("devtools://")) return "browser_internal";
  if (extensionId && value.startsWith(`chrome-extension://${extensionId}/`)) return "probe_extension";
  if (value.startsWith("chrome-extension://")) return "component_extension";
  try { const url = new URL(value); if (url.origin === syntheticOrigin) return url.pathname === "/" ? "synthetic" : "unexpected_synthetic_path"; return ["http:", "https:"].includes(url.protocol) ? "unexpected_external" : "browser_internal"; } catch { return "missing"; }
}
async function waitForPort(profile, signal) {
  for (let attempt = 0; attempt < 40; attempt += 1) { if (signal.aborted) fail("startup-aborted"); try { return (await readFile(join(profile, "DevToolsActivePort"), "utf8")).trim().split("\n"); } catch { await sleep(100); } }
  fail("devtools-active-port-unavailable");
}
async function startServer(cert, key) {
  const requests = [];
  server = createServer({ cert, key }, (request, response) => { requests.push({ method: fixedMethod(request.method), path: request.url === "/" ? "/" : "other" }); response.writeHead(200, { "content-type": "text/html" }); response.end("<!doctype html><head><link rel=\"icon\" href=\"data:,\"></head><body data-qb-marker=\"quotabar-synthetic\"></body>"); });
  await new Promise((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
  return { port: server.address().port, requests };
}
async function evaluate(call, sessionId, expression, contextId) { const result = await call("Runtime.evaluate", { expression, contextId, returnByValue: true, awaitPromise: true }, sessionId); return result.result?.result?.value; }
async function closeSession(call, sessionId) { if (sessionId) await call("Target.detachFromTarget", { sessionId }).catch(() => undefined); }

try {
  root = await mkdtemp(join(tmpdir(), "quotabar-c1-"));
  const profile = join(root, "profile"), certPath = join(root, "cert.pem"), keyPath = join(root, "key.pem");
  await withinOverall(execFileAsync("openssl", ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", keyPath, "-out", certPath, "-subj", "/CN=claude.ai", "-addext", "subjectAltName=DNS:claude.ai", "-days", "1"], { signal: aborter.signal }).catch((error) => normalizeAbort(error, aborter.signal, "harness:openssl-aborted")), "certificate-timeout");
  const synthetic = await startServer(await readFile(certPath), await readFile(keyPath));
  const args = ["--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--disable-component-update", "--disable-sync", "--no-pings", "--ignore-certificate-errors", "--host-resolver-rules=MAP * 127.0.0.1, EXCLUDE localhost", "about:blank"];
  child = spawn(chrome, args, { stdio: ["ignore", "ignore", "pipe"] });
  const earlyExit = new Promise((_, reject) => { child.once("error", () => reject(new Error("harness:chrome-spawn-error"))); child.once("exit", () => reject(new Error("harness:chrome-early-exit"))); }); void earlyExit.catch(() => undefined);
  const startup = (promise) => raceStartup(promise, earlyExit);
  const [port, wsPath] = await startup(withinOverall(waitForPort(profile, aborter.signal), "devtools-timeout"));
  rootSocket = await startup(withinOverall(openWebSocket(WebSocket, `ws://127.0.0.1:${port}${wsPath}`, 2_000, aborter.signal), "websocket-timeout"));
  const contexts = []; const frames = []; const exceptions = []; const postOrder = []; const requests = []; const targetsSeen = []; let extensionId; const syntheticOrigin = `https://claude.ai:${synthetic.port}`;
  const record = (array, value) => { if (!finalized) array.push(value); };
  client = createCdpClient(rootSocket, (event) => { if (finalized) return; if (event.method === "Runtime.executionContextCreated") record(contexts, event.params.context); if (event.method === "Page.frameNavigated") record(frames, event.params.frame); if (event.method === "Runtime.exceptionThrown") { const value = sanitizedException(event.params.exceptionDetails); if (value) record(exceptions, value); } if (event.method === "Network.requestWillBeSent") { const category = classifyUrl(event.params.request?.url, syntheticOrigin, extensionId); record(requests, { category, method: fixedMethod(event.params.request?.method), path: category === "synthetic" ? "/" : "none" }); } if (["Target.targetCreated", "Target.targetInfoChanged"].includes(event.method)) { const target = event.params.targetInfo; record(targetsSeen, { type: ["page", "worker", "service_worker", "shared_worker", "background_page"].includes(target?.type) ? target.type : "other", category: classifyUrl(target?.url, syntheticOrigin, extensionId) }); } }, aborter.signal);
  const call = client.call;
  mainRun = (async () => {
    await call("Target.setDiscoverTargets", { discover: true });
    const load = await call("Extensions.loadUnpacked", { path: probe }); if (!load.result?.id) fail("extension-load"); extensionId = load.result.id;
    const extension = (await call("Extensions.getExtensions")).result?.extensions?.find((item) => item.id === extensionId);
    if (!extension || extension.name !== expectedExtension.name || extension.version !== expectedExtension.version || extension.enabled !== true) fail("extension-identity");
    const created = await call("Target.createTarget", { url: "about:blank" }); createdTargets.push(created.result.targetId); const page = await call("Target.attachToTarget", { targetId: created.result.targetId, flatten: true }); attached.push(page.result.sessionId);
    for (const method of ["Runtime.enable", "Page.enable", "Log.enable", "Network.enable"]) await call(method, {}, page.result.sessionId);
    await call("Page.addScriptToEvaluateOnNewDocument", { source: "(() => { const post = window.postMessage.bind(window); const allowed = new Set(['quotabar-local-probe-stage','quotabar-local-probe-main-ready','quotabar-local-probe-ready-request']); window.postMessage = (data, origin) => { if (allowed.has(data?.type)) { const value = data.type === 'quotabar-local-probe-stage' ? data.stage : data.type === 'quotabar-local-probe-main-ready' ? 'main_ready' : 'ready_request'; const prior = globalThis.__qbOrder || []; if (prior.length < 4) globalThis.__qbOrder = [...prior, value]; } return post(data, origin); }; })();" }, page.result.sessionId);
    const contextStart = contexts.length; const navigation = await call("Page.navigate", { url: `https://claude.ai:${synthetic.port}/` }, page.result.sessionId); if (navigation.error) fail("synthetic-navigation");
    const end = overallDeadlineAt; let frameId;
    while (Date.now() < end) { if (aborter.signal.aborted) fail("run-aborted"); frameId = frames.find((frame) => frame.url === `https://claude.ai:${synthetic.port}/`)?.id; if (frameId && contexts.slice(contextStart).filter((item) => item.auxData?.frameId === frameId).length >= 2) break; await sleep(50); }
    if (!frameId) fail("synthetic-main-frame");
    const main = contexts.slice(contextStart).find((item) => item.auxData?.frameId === frameId && item.auxData?.isDefault === true); const isolated = contexts.slice(contextStart).find((item) => item.auxData?.frameId === frameId && item.auxData?.type === "isolated"); if (!main || !isolated) fail("worlds-unavailable");
    const [origin, marker, order] = await Promise.all([evaluate(call, page.result.sessionId, "location.origin", main.id), evaluate(call, page.result.sessionId, "document.body.dataset.qbMarker", main.id), evaluate(call, page.result.sessionId, "JSON.stringify(globalThis.__qbOrder || [])", main.id)]); postOrder.push(...JSON.parse(order || "[]"));
    const targets = (await call("Target.getTargets")).result?.targetInfos || []; const worker = targets.find((target) => target.type === "service_worker" && target.url.startsWith(`chrome-extension://${extension.id}/`)); if (!worker) fail("extension-worker-unavailable"); const workerSession = await call("Target.attachToTarget", { targetId: worker.targetId, flatten: true }); attached.push(workerSession.result.sessionId); await call("Runtime.enable", {}, workerSession.result.sessionId);
    const readView = async () => JSON.parse(await evaluate(call, workerSession.result.sessionId, "chrome.storage.session.get('quotabarProbeState').then(({ quotabarProbeState: state }) => JSON.stringify({ buildId: globalThis.QuotaBarProbeCore.BUILD_ID, diagnostic: state?.diagnostics?.[state?.slot], bootstrap: state?.bootstrap?.[state?.slot] }))")); let view = await readView(); const readinessEnd = Date.now() + 1_000; while (view.diagnostic !== "bridge_received_main_ready" && Date.now() < readinessEnd) { if (aborter.signal.aborted) fail("run-aborted"); await sleep(50); view = await readView(); }
    const classifyTarget = (target) => ({ type: ["page", "worker", "service_worker", "shared_worker", "background_page"].includes(target?.type) ? target.type : "other", category: target?.url === "about:blank" ? "browser_internal" : classifyUrl(target?.url, syntheticOrigin, extensionId) });
    const targetSummary = [...new Set([...targetsSeen, ...targets.map(classifyTarget)].map((target) => `${target.type}:${target.category}`))].sort();
    const acceptedTargetCategories = new Set(["synthetic", "probe_extension", "component_extension", "browser_internal"]);
    const unexpectedTarget = targetSummary.some((entry) => !acceptedTargetCategories.has(entry.split(":").slice(1).join(":")));
    const unexpectedRequest = requests.some((request) => !["synthetic", "probe_extension", "browser_internal"].includes(request.category));
    const externalRequest = requests.some((request) => request.category === "unexpected_external");
    const targetObserved = targetsSeen.some((target) => target.type === "page" && target.category === "synthetic") && targetSummary.some((entry) => entry.endsWith(":probe_extension"));
    const pageSameFrameObservationComplete = requests.some((request) => request.category === "synthetic");
    const report = { extension: { id: extension.id, name: extension.name, version: extension.version, enabled: extension.enabled }, synthetic: { origin: origin === syntheticOrigin ? "exact" : "mismatch", marker: marker === "quotabar-synthetic" }, buildIdExpected: "p4b-1e-r", sessionView: view, postOrder, exceptions, requests, targetSummary, pageSameFrameObservationComplete, targetObserved, unexpectedRequest, unexpectedTarget, externalRequest };
    lastReport = report; assertSmokeReport(report);
  })();
  await raceStartup(withinOverall(mainRun, "overall-deadline"), earlyExit);
} catch (error) { primaryFailure = error instanceof Error ? error.message : "unknown"; process.exitCode = 1; }
finally {
  const cleanupEndsAt = Date.now() + 4_000;
  const cleanupWithin = (promise, code) => withDeadline(promise, Math.max(1, cleanupEndsAt - Date.now()), `harness:cleanup-${code}`);
  const result = await coordinateCleanup({
    primaryFailure,
    gracefulTimeoutMs: 1_000,
    gracefulClose: !primaryFailure && client && !client.closed ? async () => { for (const targetId of createdTargets.reverse()) await client.call("Target.closeTarget", { targetId }).catch(() => undefined); for (const sessionId of attached.reverse()) await closeSession(client.call, sessionId); } : undefined,
    freeze,
    terminateChild: async () => { if (child) await terminateChild(child, sleep, 500, 500); },
    settleMain: async () => { if (mainRun) await cleanupWithin(mainRun.catch(() => undefined), "main-settlement"); },
    postChildCleanup: async () => { if (server) { server.closeAllConnections?.(); await cleanupWithin(new Promise((resolveClose, rejectClose) => server.close((error) => error ? rejectClose(error) : resolveClose())), "server"); } if (root?.startsWith(`${resolve(tmpdir())}/quotabar-c1-`)) await cleanupWithin(rm(root, { recursive: true, force: true, maxRetries: 3, retryDelay: 200 }), "temp"); else fail("cleanup-root"); },
  });
  const cleanup = result.cleanup;
  if (primaryFailure) process.stdout.write(`${JSON.stringify({ gate: "R2", verdict: "blocked", failure: primaryFailure, cleanup, postOrder: lastReport?.postOrder || [], targetSummary: lastReport?.targetSummary || [] })}\n`);
  else if (cleanup !== "complete") { process.stdout.write(`${JSON.stringify({ gate: "R2", verdict: "blocked", failure: "harness:cleanup-partial", cleanup })}\n`); process.exitCode = 1; }
  else if (lastReport) process.stdout.write(`${JSON.stringify({ gate: "R2", ...lastReport, cleanup })}\n`);
}
