(() => {
  const keys = new Set(["version", "probeSlot", "planClass", "source", "observedAt", "windows", "status", "errorCode"]);
  const windowKeys = new Set(["kind", "usedPercent", "resetAt"]);
  const forbidden = new Set(["route", "organization", "organizationId", "account", "accountId", "user", "userId", "email", "token", "cookie", "authorization", "session", "conversation", "messageId", "requestId", "traceId", "headers", "body", "url", "path", "payload", "text", "thinking", "toolInput"]);
  const timestamp = (value) => typeof value === "string" && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/.test(value);
  const object = (value, allowed) => value !== null && typeof value === "object" && !Array.isArray(value) && Object.keys(value).every((key) => allowed.has(key));
  const forbiddenKey = (value) => value !== null && typeof value === "object" && (Array.isArray(value) ? value.some(forbiddenKey) : Object.entries(value).some(([key, child]) => forbidden.has(key) || forbiddenKey(child)));
  const validWindow = (value) => object(value, windowKeys) && ["five_hour", "weekly"].includes(value.kind) && (!("usedPercent" in value) || (typeof value.usedPercent === "number" && Number.isFinite(value.usedPercent) && value.usedPercent >= 0 && value.usedPercent <= 100)) && (!("resetAt" in value) || timestamp(value.resetAt));
  const valid = (value) => object(value, keys) && !forbiddenKey(value) && value.version === "v1" && ["profile-a", "profile-b"].includes(value.probeSlot) && ["paid", "free", "unknown"].includes(value.planClass) && ["usage_endpoint", "completion_sse"].includes(value.source) && timestamp(value.observedAt) && Array.isArray(value.windows) && value.windows.every(validWindow) && ["available", "unavailable", "malformed"].includes(value.status) && (!("errorCode" in value) || ["unavailable", "malformed_payload", "redacted_field", "overflow"].includes(value.errorCode)) && (value.status === "available" ? value.windows.length > 0 && !("errorCode" in value) : value.windows.length === 0 && "errorCode" in value);
  globalThis.QuotaBarProbeValidator = { safeEnvelope: (value) => valid(value) ? structuredClone(value) : null };
})();
