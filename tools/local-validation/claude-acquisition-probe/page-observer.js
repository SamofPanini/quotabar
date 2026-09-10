(() => {
  "use strict";
  const core = globalThis.QuotaBarProbeCore, post = (data) => window.postMessage({ ...data, buildId: core.BUILD_ID }, location.origin), ready = () => post({ type: "quotabar-local-probe-main-ready" }), stage = (value) => post({ type: "quotabar-local-probe-stage", stage: value });
  const emit = (output) => post({ type: "quotabar-local-probe-observation", output });
  const fail = (source, errorCode) => ({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source, observedAt: new Date().toISOString(), windows: [], status: errorCode === "unavailable" ? "unavailable" : "malformed", errorCode });
  async function endpoint(response) { const r = await core.drainEndpoint(response); if (r.kind === "found" && r.windows.length) emit({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source: "usage_endpoint", observedAt: new Date().toISOString(), windows: r.windows, status: "available" }); else emit(fail("usage_endpoint", r.kind === "overflow" ? "overflow" : r.kind === "found" || r.kind === "none" ? "unavailable" : "malformed_payload")); }
  async function sse(response) { const r = await core.parseMessageLimitSse(response); if (r.kind === "found") { stage("message_limit_found"); emit({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source: "completion_sse", observedAt: new Date().toISOString(), windows: r.windows, status: "available" }); } else { stage(r.kind === "none" ? "completion_seen_no_message_limit" : "completion_parse_failed"); if (r.kind !== "none") emit(fail("completion_sse", r.kind === "overflow" ? "overflow" : "malformed_payload")); } }
  window.addEventListener("message", (event) => { const data = core.validWindowMessage(event, window, location.origin); if (data?.type === "quotabar-local-probe-ready-request") { stage("observer_installed"); ready(); } });
  core.installFetchObserver(window, location.origin, endpoint, sse, stage); stage("observer_installed"); ready();
})();
