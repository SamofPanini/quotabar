export const PROBE_SLOTS = Object.freeze(["profile-a", "profile-b"]);
export const ERROR_CODES = Object.freeze(["unavailable", "malformed_payload", "redacted_field", "overflow"]);

const ENVELOPE_KEYS = new Set(["version", "probeSlot", "planClass", "source", "observedAt", "windows", "status", "errorCode"]);
const WINDOW_KEYS = new Set(["kind", "usedPercent", "resetAt"]);
const FORBIDDEN_KEYS = new Set([
  "route", "organization", "organizationId", "account", "accountId", "user", "userId", "email",
  "token", "cookie", "authorization", "session", "conversation", "messageId", "requestId", "traceId",
  "headers", "body", "url", "path", "payload", "text", "thinking", "toolInput"
]);

function hasOnlyKeys(value, allowed) {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    && Object.keys(value).every((key) => allowed.has(key));
}

function hasForbiddenKey(value) {
  if (value === null || typeof value !== "object") return false;
  if (Array.isArray(value)) return value.some(hasForbiddenKey);
  return Object.entries(value).some(([key, child]) => FORBIDDEN_KEYS.has(key) || hasForbiddenKey(child));
}

function isTimestamp(value) {
  return typeof value === "string" && /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/.test(value);
}

function isWindow(window) {
  if (!hasOnlyKeys(window, WINDOW_KEYS)) return false;
  if (window.kind !== "five_hour" && window.kind !== "weekly") return false;
  if ("usedPercent" in window && (typeof window.usedPercent !== "number" || !Number.isFinite(window.usedPercent) || window.usedPercent < 0 || window.usedPercent > 100)) return false;
  return !("resetAt" in window) || isTimestamp(window.resetAt);
}

/** Strict boundary validator: unknown or identity-bearing values cannot leave page world. */
export function isSanitizedEnvelope(value) {
  if (!hasOnlyKeys(value, ENVELOPE_KEYS) || hasForbiddenKey(value)) return false;
  if (value.version !== "v1" || !PROBE_SLOTS.includes(value.probeSlot)) return false;
  if (!["paid", "free", "unknown"].includes(value.planClass)) return false;
  if (!["usage_endpoint", "completion_sse"].includes(value.source)) return false;
  if (!isTimestamp(value.observedAt) || !Array.isArray(value.windows) || !value.windows.every(isWindow)) return false;
  if (!["available", "unavailable", "malformed"].includes(value.status)) return false;
  if ("errorCode" in value && !ERROR_CODES.includes(value.errorCode)) return false;
  if (value.status === "available") return value.windows.length > 0 && !("errorCode" in value);
  return value.windows.length === 0 && "errorCode" in value;
}

export function safeEnvelope(input) {
  return isSanitizedEnvelope(input) ? structuredClone(input) : null;
}

export function unavailable(source, planClass, errorCode) {
  return {
    version: "v1", probeSlot: "profile-a", planClass, source, observedAt: new Date().toISOString(),
    windows: [], status: errorCode === "malformed_payload" || errorCode === "redacted_field" || errorCode === "overflow" ? "malformed" : "unavailable", errorCode
  };
}

export class SlotStore {
  #observations = new Map();

  write(envelope) {
    const safe = safeEnvelope(envelope);
    if (!safe) return false;
    const existing = this.#observations.get(safe.probeSlot);
    // A verified endpoint observation wins over a later event-driven observation.
    if (existing?.status === "available" && existing.source === "usage_endpoint" && safe.source === "completion_sse") return true;
    this.#observations.set(safe.probeSlot, safe);
    return true;
  }

  read(slot) {
    return PROBE_SLOTS.includes(slot) ? safeEnvelope(this.#observations.get(slot)) : null;
  }
}
