import { describe, expect, test } from "vitest";
import { SlotStore, isSanitizedEnvelope, safeEnvelope } from "./shared.js";
import { endpointWindows, isCompletionUrl, MAX_SSE_LINE_BYTES, observeWithoutInterference, parseMessageLimitSse } from "./probe-core.js";

const at = "2026-01-02T03:04:05Z";
const paid = { version: "v1", probeSlot: "profile-a", planClass: "paid", source: "usage_endpoint", observedAt: at, windows: [{ kind: "five_hour", usedPercent: 25, resetAt: at }, { kind: "weekly", usedPercent: 50, resetAt: at }], status: "available" };
const free = { version: "v1", probeSlot: "profile-b", planClass: "free", source: "completion_sse", observedAt: at, windows: [{ kind: "five_hour", usedPercent: 10, resetAt: at }], status: "available" };

describe("sanitized envelope", () => {
  test("accepts paid endpoint five-hour and weekly windows", () => expect(isSanitizedEnvelope(paid)).toBe(true));
  test("accepts free SSE permitted windows without inventing an empty endpoint window", () => {
    expect(isSanitizedEnvelope(free)).toBe(true);
    expect(isSanitizedEnvelope({ ...free, windows: [], status: "available" })).toBe(false);
  });
  test("fails closed for malformed and unknown fields", () => {
    expect(isSanitizedEnvelope({ ...paid, windows: [{ kind: "monthly" }] })).toBe(false);
    expect(isSanitizedEnvelope({ ...paid, extra: true })).toBe(false);
  });
  test("rejects forbidden keys and synthetic sentinel values cannot cross the boundary", () => {
    for (const key of ["route", "organizationId", "email", "token", "cookie", "authorization", "conversation", "messageId", "requestId"]) {
      expect(safeEnvelope({ ...paid, [key]: "synthetic-sentinel" })).toBeNull();
    }
    expect(safeEnvelope({ ...paid, planClass: "synthetic-sentinel" })).toBeNull();
  });
  test("unavailable and malformed retain missing windows rather than zero", () => {
    expect(isSanitizedEnvelope({ ...paid, windows: [], status: "unavailable", errorCode: "unavailable" })).toBe(true);
    expect(isSanitizedEnvelope({ ...paid, windows: [{ kind: "five_hour", usedPercent: 0 }], status: "unavailable", errorCode: "unavailable" })).toBe(false);
  });
});

describe("isolated synthetic slots", () => {
  test("never overwrites or inherits between slots", () => {
    const store = new SlotStore();
    expect(store.write(paid)).toBe(true);
    expect(store.write(free)).toBe(true);
    expect(store.read("profile-a")).toEqual(paid);
    expect(store.read("profile-b")).toEqual(free);
  });
  test("paid endpoint data outranks SSE for the same slot", () => {
    const store = new SlotStore();
    store.write({ ...paid, probeSlot: "profile-a" });
    store.write({ ...free, probeSlot: "profile-a" });
    expect(store.read("profile-a")?.source).toBe("usage_endpoint");
  });
});

describe("bounded acquisition parsing", () => {
  test("parses paid endpoint fixture and leaves an empty free endpoint absent", () => {
    expect(endpointWindows({ five_hour: { utilization: 25, resets_at: at }, seven_day: { utilization: 50, resets_at: at } })).toEqual([{ kind: "five_hour", usedPercent: 25, resetAt: at }, { kind: "weekly", usedPercent: 50, resetAt: at }]);
    expect(endpointWindows({})).toEqual([]);
  });
  test("parses real-shaped 5h/7d message_limit fractions and Unix resets", async () => {
    const response = new Response('data: {"type":"other_event","metric":1}\n\ndata: {"type":"message_limit","windows":{"5h":{"utilization":0.25,"resets_at":1767323045},"7d":{"utilization":0.5,"resets_at":1767409445}}}\n');
    await expect(parseMessageLimitSse(response)).resolves.toEqual({ kind: "found", windows: [{ kind: "five_hour", usedPercent: 25, resetAt: "2026-01-02T03:04:05.000Z" }, { kind: "weekly", usedPercent: 50, resetAt: "2026-01-03T03:04:05.000Z" }] });
  });
  test("uses 100 percent for exceeded_limit even below one", async () => {
    const response = new Response('data: {"type":"message_limit","windows":{"5h":{"utilization":0.1,"status":"exceeded_limit","resets_at":1767323045}}}\n');
    await expect(parseMessageLimitSse(response)).resolves.toEqual({ kind: "found", windows: [{ kind: "five_hour", usedPercent: 100, resetAt: "2026-01-02T03:04:05.000Z" }] });
  });
  test("matches only exact same-origin completion and retry_completion paths", () => {
    const origin = "https://claude.ai";
    expect(isCompletionUrl("/api/organizations/synthetic/chat_conversations/synthetic/completion", origin)).toBe(true);
    expect(isCompletionUrl("/api/organizations/synthetic/chat_conversations/synthetic/retry_completion", origin)).toBe(true);
    expect(isCompletionUrl("/api/organizations/synthetic/chat_conversations/synthetic/retry-completion", origin)).toBe(false);
    expect(isCompletionUrl("/api/organizations/synthetic/completion", origin)).toBe(false);
    expect(isCompletionUrl("https://example.invalid/api/organizations/synthetic/chat_conversations/synthetic/completion", origin)).toBe(false);
  });
  test("fails closed on malformed candidate and bounded parser overflow", async () => {
    await expect(parseMessageLimitSse(new Response('data: {"type":"message_limit"}\n'))).resolves.toEqual({ kind: "malformed" });
    await expect(parseMessageLimitSse(new Response(`data: ${"x".repeat(MAX_SSE_LINE_BYTES + 1)}\n`))).resolves.toEqual({ kind: "overflow" });
  });
  test("observation returns the original response without blocking or mutation", async () => {
    const response = new Response("synthetic"); let seen = false;
    expect(observeWithoutInterference(response, async (copy) => { seen = await copy.text() === "synthetic"; })).toBe(response);
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(await response.text()).toBe("synthetic"); expect(seen).toBe(true);
  });
});

test("manifest uses MAIN-world injection without web-accessible resources", async () => {
  const manifest = await import("./manifest.json", { with: { type: "json" } });
  expect(manifest.default.web_accessible_resources).toBeUndefined();
  expect(manifest.default.content_scripts).toContainEqual(expect.objectContaining({ js: ["page-observer.js"], world: "MAIN", run_at: "document_start" }));
});
