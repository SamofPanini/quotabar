(() => {
  "use strict";
  const MAX_ENDPOINT_BYTES = 32 * 1024;
  const MAX_SSE_LINE_BYTES = 64 * 1024;
  const eventName = "quotabar-local-probe-observation";
  const timestamp = () => new Date().toISOString();
  const emit = (value) => window.dispatchEvent(new CustomEvent(eventName, { detail: value }));
  const failure = (source, errorCode) => ({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source, observedAt: timestamp(), windows: [], status: errorCode === "unavailable" ? "unavailable" : "malformed", errorCode });
  const percent = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 100 ? value : undefined;
  const fraction = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1 ? value : undefined;
  const endpointTimestamp = (value) => typeof value === "string" && /^\d{4}-\d{2}-\d{2}T/.test(value) ? value : undefined;
  const sseTimestamp = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 ? new Date(value * 1000).toISOString() : undefined;
  const endpointWindow = (raw, kind) => {
    if (raw === null || typeof raw !== "object" || Array.isArray(raw)) return null;
    const usedPercent = percent(raw.utilization); const resetAt = endpointTimestamp(raw.resets_at);
    return usedPercent === undefined && resetAt === undefined ? null : { kind, ...(usedPercent === undefined ? {} : { usedPercent }), ...(resetAt === undefined ? {} : { resetAt }) };
  };
  const sseWindow = (raw, kind) => {
    if (raw === null || typeof raw !== "object" || Array.isArray(raw)) return null;
    const value = fraction(raw.utilization); const usedPercent = raw.status === "exceeded_limit" ? 100 : value === undefined ? undefined : value * 100; const resetAt = sseTimestamp(raw.resets_at);
    return usedPercent === undefined && resetAt === undefined ? null : { kind, ...(usedPercent === undefined ? {} : { usedPercent }), ...(resetAt === undefined ? {} : { resetAt }) };
  };
  const endpointWindows = (raw) => raw === null || typeof raw !== "object" || Array.isArray(raw) ? null : [endpointWindow(raw.five_hour, "five_hour"), endpointWindow(raw.seven_day, "weekly")].filter(Boolean);
  const sameOrigin = (url, pattern) => { try { const parsed = new URL(url, location.origin); return parsed.origin === location.origin && pattern.test(parsed.pathname); } catch { return false; } };
  const isUsageUrl = (url) => sameOrigin(url, /^\/api\/organizations\/[^/]+\/usage$/);
  const isCompletionUrl = (url) => sameOrigin(url, /^\/api\/organizations\/[^/]+\/chat_conversations\/[^/]+\/(?:retry_)?completion$/);
  const observeWithoutInterference = (response, observe) => { void observe(response.clone()).catch(() => undefined); return response; };

  async function endpoint(response) {
    try {
      const length = Number(response.headers.get("content-length"));
      if (Number.isFinite(length) && length > MAX_ENDPOINT_BYTES) throw new Error("cap");
      const text = await response.text();
      if (new TextEncoder().encode(text).byteLength > MAX_ENDPOINT_BYTES) throw new Error("cap");
      const windows = endpointWindows(JSON.parse(text));
      if (windows === null) return emit(failure("usage_endpoint", "malformed_payload"));
      if (windows.length) emit({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source: "usage_endpoint", observedAt: timestamp(), windows, status: "available" });
    } catch (error) { emit(failure("usage_endpoint", error instanceof Error && error.message === "cap" ? "overflow" : "malformed_payload")); }
  }

  async function sse(response) {
    if (!response.body) return;
    const reader = response.body.getReader(); const decoder = new TextDecoder(); let remainder = "";
    try {
      while (true) {
        const { done, value } = await reader.read(); remainder += decoder.decode(value || new Uint8Array(), { stream: !done });
        if (new TextEncoder().encode(remainder).byteLength > MAX_SSE_LINE_BYTES && !remainder.includes("\n")) return emit(failure("completion_sse", "overflow"));
        let newline;
        while ((newline = remainder.indexOf("\n")) !== -1) {
          const line = remainder.slice(0, newline).replace(/\r$/, ""); remainder = remainder.slice(newline + 1);
          if (new TextEncoder().encode(line).byteLength > MAX_SSE_LINE_BYTES) return emit(failure("completion_sse", "overflow"));
          if (!line.startsWith("data:") || !/"type"\s*:\s*"message_limit"/.test(line)) continue;
          let raw; try { raw = JSON.parse(line.slice(5)); } catch { return emit(failure("completion_sse", "malformed_payload")); }
          if (raw?.type !== "message_limit" || raw.windows === null || typeof raw.windows !== "object" || Array.isArray(raw.windows)) return emit(failure("completion_sse", "malformed_payload"));
          const windows = [sseWindow(raw.windows["5h"], "five_hour"), sseWindow(raw.windows["7d"], "weekly")].filter(Boolean);
          return windows.length ? emit({ version: "v1", probeSlot: "profile-a", planClass: "unknown", source: "completion_sse", observedAt: timestamp(), windows, status: "available" }) : emit(failure("completion_sse", "malformed_payload"));
        }
        if (done) return;
      }
    } finally { reader.releaseLock(); }
  }

  const originalFetch = window.fetch;
  window.fetch = function (...args) {
    const request = args[0]; const url = typeof request === "string" ? request : request?.url;
    const method = (args[1]?.method ?? (typeof request === "string" ? "GET" : request?.method ?? "GET")).toUpperCase();
    const result = originalFetch.apply(this, args);
    if (method === "GET" && isUsageUrl(url)) result.then((response) => observeWithoutInterference(response, endpoint)).catch(() => undefined);
    if (isCompletionUrl(url)) result.then((response) => observeWithoutInterference(response, sse)).catch(() => undefined);
    return result;
  };
})();
