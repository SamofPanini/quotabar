import { readFile, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const directory = new URL(".", import.meta.url);
const core = await readFile(new URL("probe-core.js", directory), "utf8");
const coreForEntry = core.replace("globalThis.QuotaBarProbeCore = createProbeCore();", "const core = createProbeCore();");
if (coreForEntry === core) throw new Error("authoritative-core-marker-missing");
const preamble = "/* Generated from probe-core.js by generate-entries.mjs; do not edit. */\n";
const entries = {
  "isolated-entry.js": `${preamble}(() => {\n${coreForEntry}\n(() => {\n  const send = (value) => chrome.runtime.sendMessage(value);\n  const post = (type) => window.postMessage({ type, buildId: core.BUILD_ID }, location.origin);\n  send({ type: "stage", stage: "bridge_loaded" });\n  window.addEventListener("message", (event) => {\n    const data = core.validWindowMessage(event, window, location.origin);\n    if (!data) return;\n    if (data.type === "quotabar-local-probe-main-ready") send({ type: "stage", stage: "bridge_received_main_ready" });\n    else if (data.type === "quotabar-local-probe-stage") send({ type: "stage", stage: data.stage });\n    else if (data.type === "quotabar-local-probe-observation") send({ type: "observation", output: data.output });\n  });\n  post("quotabar-local-probe-ready-request");\n})();\n})();\n`,
  "main-entry.js": `${preamble}(() => {\n${coreForEntry}\n(() => {\n  "use strict";\n  const post = (data) => window.postMessage({ ...data, buildId: core.BUILD_ID }, location.origin);\n  const ready = () => post({ type: "quotabar-local-probe-main-ready" });\n  const stage = (value) => post({ type: "quotabar-local-probe-stage", stage: value });\n  const emit = (output) => post({ type: "quotabar-local-probe-observation", output });\n  const fail = (source, errorCode) => ({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source, observedAt: new Date().toISOString(), windows: [], status: errorCode === "unavailable" ? "unavailable" : "malformed", errorCode });\n  async function endpoint(response) { const result = await core.drainEndpoint(response); if (result.kind === "found" && result.windows.length) emit({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source: "usage_endpoint", observedAt: new Date().toISOString(), windows: result.windows, status: "available" }); else emit(fail("usage_endpoint", result.kind === "overflow" ? "overflow" : result.kind === "found" || result.kind === "none" ? "unavailable" : "malformed_payload")); }\n  async function sse(response) { const result = await core.parseMessageLimitSse(response); if (result.kind === "found") { stage("message_limit_found"); emit({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source: "completion_sse", observedAt: new Date().toISOString(), windows: result.windows, status: "available" }); } else { stage(result.kind === "none" ? "completion_seen_no_message_limit" : "completion_parse_failed"); if (result.kind !== "none") emit(fail("completion_sse", result.kind === "overflow" ? "overflow" : "malformed_payload")); } }\n  window.addEventListener("message", (event) => { const data = core.validWindowMessage(event, window, location.origin); if (data?.type === "quotabar-local-probe-ready-request") { stage("observer_installed"); ready(); } });\n  core.installFetchObserver(window, location.origin, endpoint, sse, stage);\n  stage("observer_installed");\n  ready();\n})();\n})();\n`,
};
const check = process.argv.includes("--check");
for (const [name, output] of Object.entries(entries)) {
  const url = new URL(name, directory);
  if (check) {
    if (await readFile(url, "utf8") !== output) throw new Error(`generated-entry-drift:${name}`);
  } else await writeFile(url, output);
}
