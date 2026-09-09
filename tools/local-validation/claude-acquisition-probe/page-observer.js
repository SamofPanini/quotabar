(() => {
  "use strict";
  const core = globalThis.QuotaBarProbeCore, name = "quotabar-local-probe-observation";
  const emit = (detail) => window.dispatchEvent(new CustomEvent(name, { detail }));
  const fail = (source, errorCode) => ({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source, observedAt: new Date().toISOString(), windows: [], status: errorCode === "unavailable" ? "unavailable" : "malformed", errorCode });
  async function endpoint(response) { const r = await core.drainEndpoint(response); if (r.kind === "found" && r.windows.length) emit({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source: "usage_endpoint", observedAt: new Date().toISOString(), windows: r.windows, status: "available" }); else emit(fail("usage_endpoint", r.kind === "overflow" ? "overflow" : r.kind === "found" || r.kind === "none" ? "unavailable" : "malformed_payload")); }
  async function sse(response) { const r = await core.parseMessageLimitSse(response); if (r.kind === "found") emit({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source: "completion_sse", observedAt: new Date().toISOString(), windows: r.windows, status: "available" }); else if (r.kind !== "none") emit(fail("completion_sse", r.kind === "overflow" ? "overflow" : "malformed_payload")); }
  core.installFetchObserver(window, location.origin, endpoint, sse);
})();
