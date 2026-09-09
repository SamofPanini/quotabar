export const MAX_SSE_LINE_BYTES = 64 * 1024;

const endpointPercent = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 100 ? value : undefined;
const sseFraction = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1 ? value : undefined;
const isoTimestamp = (value) => typeof value === "string" && /^\d{4}-\d{2}-\d{2}T/.test(value) ? value : undefined;
const unixTimestamp = (value) => typeof value === "number" && Number.isFinite(value) && value >= 0 ? new Date(value * 1000).toISOString() : undefined;

function endpointWindow(raw, kind) {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) return null;
  const usedPercent = endpointPercent(raw.utilization);
  const resetAt = isoTimestamp(raw.resets_at);
  return usedPercent === undefined && resetAt === undefined ? null : { kind, ...(usedPercent === undefined ? {} : { usedPercent }), ...(resetAt === undefined ? {} : { resetAt }) };
}

function sseWindow(raw, kind) {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) return null;
  const fraction = sseFraction(raw.utilization);
  const usedPercent = raw.status === "exceeded_limit" ? 100 : fraction === undefined ? undefined : fraction * 100;
  const resetAt = unixTimestamp(raw.resets_at);
  return usedPercent === undefined && resetAt === undefined ? null : { kind, ...(usedPercent === undefined ? {} : { usedPercent }), ...(resetAt === undefined ? {} : { resetAt }) };
}

/** Paid endpoint values are already percentages and use ISO reset timestamps. */
export function endpointWindows(raw) {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) return null;
  return [endpointWindow(raw.five_hour, "five_hour"), endpointWindow(raw.seven_day, "weekly")].filter(Boolean);
}

/** Exact same-origin completion or retry_completion pathname matcher. */
export function isCompletionUrl(url, origin) {
  try {
    const parsed = new URL(url, origin);
    return parsed.origin === origin && /^\/api\/organizations\/[^/]+\/chat_conversations\/[^/]+\/(?:retry_)?completion$/.test(parsed.pathname);
  } catch { return false; }
}

/** Reads bounded lines and parses JSON only for a message_limit candidate. */
export async function parseMessageLimitSse(response) {
  if (!response.body) return { kind: "none" };
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let remainder = "";
  try {
    while (true) {
      const { done, value } = await reader.read();
      remainder += decoder.decode(value || new Uint8Array(), { stream: !done });
      if (new TextEncoder().encode(remainder).byteLength > MAX_SSE_LINE_BYTES && !remainder.includes("\n")) return { kind: "overflow" };
      let newline;
      while ((newline = remainder.indexOf("\n")) !== -1) {
        const line = remainder.slice(0, newline).replace(/\r$/, "");
        remainder = remainder.slice(newline + 1);
        if (new TextEncoder().encode(line).byteLength > MAX_SSE_LINE_BYTES) return { kind: "overflow" };
        if (!line.startsWith("data:") || !/"type"\s*:\s*"message_limit"/.test(line)) continue;
        let raw;
        try { raw = JSON.parse(line.slice(5)); } catch { return { kind: "malformed" }; }
        if (raw?.type !== "message_limit" || raw.windows === null || typeof raw.windows !== "object" || Array.isArray(raw.windows)) return { kind: "malformed" };
        const windows = [sseWindow(raw.windows["5h"], "five_hour"), sseWindow(raw.windows["7d"], "weekly")].filter(Boolean);
        return windows.length ? { kind: "found", windows } : { kind: "malformed" };
      }
      if (done) return { kind: "none" };
    }
  } finally { reader.releaseLock(); }
}

/** Observation returns the exact original response immediately. */
export function observeWithoutInterference(response, observe) {
  void observe(response.clone()).catch(() => undefined);
  return response;
}
