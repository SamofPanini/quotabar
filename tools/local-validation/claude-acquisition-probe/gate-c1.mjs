import { spawn, execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:https";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const sleep = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms));
const chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const probe = dirname(fileURLToPath(import.meta.url));
const fixedBuildId = "p4b-1e-r";
const deadlineMs = 8_000;
let root;
let child;
let server;

function sanitizedContext(context) {
  const aux = context.auxData || {};
  return {
    defaultWorld: aux.isDefault === true,
    type: typeof aux.type === "string" ? aux.type : "unknown",
    origin: context.origin?.startsWith("https://claude.ai:") ? "synthetic" : "other",
    named: context.name?.includes("chrome-extension://") ? "extension-named" : "unnamed",
  };
}

function allowedException(details) {
  const url = String(details.url || "");
  const source = ["page-observer.js", "content.js", "probe-core.js", "isolated-entry.js", "main-entry.js"].find((name) => url.endsWith(`/${name}`));
  if (!source) return null;
  const description = String(details.exception?.description || details.text || "");
  const property = ["validWindowMessage", "installFetchObserver"].find((value) => description.includes(value));
  return {
    source,
    line: Number.isInteger(details.lineNumber) ? details.lineNumber + 1 : null,
    class: description.includes("TypeError") ? "TypeError" : "ExtensionError",
    property: property || "none",
  };
}

async function waitForPort(profile) {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    try {
      return (await readFile(join(profile, "DevToolsActivePort"), "utf8")).trim().split("\n");
    } catch {
      await sleep(100);
    }
  }
  throw new Error("devtools-active-port-unavailable");
}

async function openSocket(url) {
  const socket = new WebSocket(url);
  await new Promise((resolveOpen, rejectOpen) => {
    socket.onopen = resolveOpen;
    socket.onerror = rejectOpen;
  });
  return socket;
}

function createCdp(socket, onEvent) {
  let nextId = 0;
  const pending = new Map();
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    if (message.id && pending.has(message.id)) {
      const resolveCall = pending.get(message.id);
      pending.delete(message.id);
      resolveCall(message);
      return;
    }
    onEvent?.(message);
  });
  return (method, params = {}, sessionId) => new Promise((resolveCall, rejectCall) => {
    const id = ++nextId;
    const timer = setTimeout(() => { pending.delete(id); rejectCall(new Error(`cdp-timeout:${method}`)); }, 5_000);
    pending.set(id, (message) => { clearTimeout(timer); resolveCall(message); });
    socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
  });
}

async function evaluate(call, sessionId, expression, contextId) {
  const result = await call("Runtime.evaluate", { expression, contextId, returnByValue: true }, sessionId);
  return result.result?.result?.value ?? "evaluation-unavailable";
}

async function startServer(cert, key) {
  let requests = 0;
  server = createServer({ cert, key }, (request, response) => {
    requests += 1;
    if (request.url === "/") {
      response.writeHead(200, { "content-type": "text/html" });
      response.end("<!doctype html><body data-qb-marker=\"quotabar-synthetic\"></body>");
      return;
    }
    response.writeHead(204);
    response.end();
  });
  await new Promise((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
  return { port: server.address().port, getRequests: () => requests };
}

try {
  root = await mkdtemp(join(tmpdir(), "quotabar-c1-"));
  const profile = join(root, "profile");
  const certPath = join(root, "cert.pem");
  const keyPath = join(root, "key.pem");
  await execFileAsync("openssl", ["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", keyPath, "-out", certPath, "-subj", "/CN=claude.ai", "-addext", "subjectAltName=DNS:claude.ai", "-days", "1"]);
  const synthetic = await startServer(await readFile(certPath), await readFile(keyPath));
  const args = [
    "--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`,
    "--no-first-run", "--no-default-browser-check", "--disable-background-networking",
    "--disable-component-update", "--disable-sync", "--no-pings", "--ignore-certificate-errors",
    "--host-resolver-rules=MAP * 127.0.0.1, EXCLUDE localhost", "about:blank",
  ];
  child = spawn(chrome, args, { stdio: ["ignore", "ignore", "pipe"] });
  let stderr = "";
  child.stderr.on("data", (chunk) => { if (stderr.length < 2000) stderr += chunk.toString(); });
  const [port, wsPath] = await waitForPort(profile);
  const rootSocket = await openSocket(`ws://127.0.0.1:${port}${wsPath}`);
  const contexts = [];
  const frames = [];
  const exceptions = [];
  const call = createCdp(rootSocket, (event) => {
    if (event.method === "Runtime.executionContextCreated") contexts.push(event.params.context);
    if (event.method === "Page.frameNavigated") frames.push(event.params.frame);
    if (event.method === "Runtime.exceptionThrown") {
      const value = allowedException(event.params.exceptionDetails);
      if (value) exceptions.push(value);
    }
  });
  const load = await call("Extensions.loadUnpacked", { path: probe });
  if (!load.result?.id) throw new Error(`extension-load-${load.error?.code || "unavailable"}`);
  const extensionId = load.result.id;
  const listed = await call("Extensions.getExtensions");
  const extension = listed.result?.extensions?.find((item) => item.id === extensionId);
  if (!extension || extension.name !== "QuotaBar local Claude acquisition probe" || extension.version !== "0.1.0" || extension.enabled !== true) throw new Error("extension-identity-unproven");
  await call("Target.setDiscoverTargets", { discover: true });
  const created = await call("Target.createTarget", { url: "about:blank" });
  const attached = await call("Target.attachToTarget", { targetId: created.result.targetId, flatten: true });
  const sessionId = attached.result.sessionId;
  await call("Runtime.enable", {}, sessionId);
  await call("Page.enable", {}, sessionId);
  await call("Log.enable", {}, sessionId);
  const navigationContextStart = contexts.length;
  const navigated = await call("Page.navigate", { url: `https://claude.ai:${synthetic.port}/` }, sessionId);
  if (navigated.error) throw new Error("synthetic-navigation-failed");
  const end = Date.now() + deadlineMs;
  let frameId;
  while (Date.now() < end) {
    frameId = frames.find((frame) => frame.url === `https://claude.ai:${synthetic.port}/`)?.id;
    if (frameId && contexts.slice(navigationContextStart).filter((item) => item.auxData?.frameId === frameId).length >= 2) break;
    await sleep(100);
  }
  if (!frameId) throw new Error("synthetic-main-frame-unavailable");
  const frameContexts = contexts.slice(navigationContextStart).filter((item) => item.auxData?.frameId === frameId);
  const main = frameContexts.find((item) => item.auxData?.isDefault === true);
  const isolated = frameContexts.find((item) => item.auxData?.isDefault === false && item.auxData?.type === "isolated");
  if (!main || !isolated) throw new Error("same-frame-worlds-not-distinguishable");
  const expressions = [
    "typeof globalThis.QuotaBarProbeCore",
    "typeof globalThis.QuotaBarProbeCore?.validWindowMessage",
    "typeof globalThis.QuotaBarProbeCore?.installFetchObserver",
  ];
  const inspect = async (context) => ({
    descriptor: sanitizedContext(context),
    core: await evaluate(call, sessionId, expressions[0], context.id),
    validWindowMessage: await evaluate(call, sessionId, expressions[1], context.id),
    installFetchObserver: await evaluate(call, sessionId, expressions[2], context.id),
  });
  const mainResult = await inspect(main);
  const isolatedResult = await inspect(isolated);
  const origin = await evaluate(call, sessionId, "location.origin", main.id);
  const marker = await evaluate(call, sessionId, "document.body.dataset.qbMarker", main.id);
  const popup = await call("Target.createTarget", { url: `chrome-extension://${extensionId}/popup.html` });
  const popupAttach = await call("Target.attachToTarget", { targetId: popup.result.targetId, flatten: true });
  await call("Runtime.enable", {}, popupAttach.result.sessionId);
  const popupState = await call("Runtime.evaluate", {
    expression: "new Promise((resolve) => chrome.runtime.sendMessage({ type: 'get-state' }, (reply) => resolve(JSON.stringify({ buildId: globalThis.QuotaBarProbeCore.BUILD_ID, diagnostic: reply?.state?.diagnostics?.[reply?.state?.slot] }))))",
    awaitPromise: true,
    returnByValue: true,
  }, popupAttach.result.sessionId);
  const sessionView = JSON.parse(popupState.result?.result?.value || "{}");
  rootSocket.close();
  process.stdout.write(`${JSON.stringify({
    gate: "C2", browserClass: "Google Chrome", rootLoad: "passed", extension: { id: extensionId, name: extension.name, version: extension.version, enabled: extension.enabled },
    synthetic: { origin: origin === `https://claude.ai:${synthetic.port}` ? "exact" : "mismatch", marker: marker === "quotabar-synthetic", loopbackRequests: synthetic.getRequests() },
    buildIdExpected: fixedBuildId, sessionView, main: mainResult, isolated: isolatedResult, exceptions, stderr: stderr ? "bounded-nonempty" : "empty",
  })}\n`);
} catch (error) {
  process.stdout.write(`${JSON.stringify({ gate: "C2", verdict: "blocked", failure: error instanceof Error ? error.message : "unknown" })}\n`);
  process.exitCode = 1;
} finally {
  if (child && !child.killed) { child.kill("SIGTERM"); await sleep(300); }
  if (server) await new Promise((resolveClose) => server.close(resolveClose));
  if (root?.startsWith(`${resolve(tmpdir())}/quotabar-c1-`)) await rm(root, { recursive: true, force: true, maxRetries: 3, retryDelay: 200 });
}
