import { describe, expect, it } from 'vitest';
import {
  claudePingWindowState,
  codexPingWindowState,
  parseRfc3339EpochSeconds,
} from '../src/services/ping_window';

describe('PING1 numbered window vectors', () => {
  const now = Date.parse('2026-10-05T11:00:00Z') / 1000;
  it('C01..C11 match the Rust Codex table and fail closed', () => {
    const vectors: Array<[string, unknown, string]> = [
      ['C01 threshold-minus-one', { usedPercent: 0, resetsAt: now + 17_939, windowMinutes: 300 }, 'open'],
      ['C02 threshold-equal', { usedPercent: 0, resetsAt: now + 17_940, windowMinutes: 300 }, 'closed'],
      ['C03 threshold-plus-one', { usedPercent: 0, resetsAt: now + 17_941, windowMinutes: 300 }, 'closed'],
      ['C04 used-positive', { usedPercent: 0.1, resetsAt: now + 18_000, windowMinutes: 300 }, 'open'],
      ['C05 expired-reset', { usedPercent: 0, resetsAt: now - 1, windowMinutes: 300 }, 'open'],
      ['C06 missing-reset', { usedPercent: 0, windowMinutes: 300 }, 'unknown'],
      ['C07 missing-window', { usedPercent: 0, resetsAt: now + 1 }, 'unknown'],
      ['C08 negative-used', { usedPercent: -0.1, resetsAt: now + 18_000, windowMinutes: 300 }, 'unknown'],
      ['C09 non-finite-used', { usedPercent: Number.NaN, resetsAt: now + 18_000, windowMinutes: 300 }, 'unknown'],
      ['C10 negative-reset', { usedPercent: 0, resetsAt: -1, windowMinutes: 300 }, 'unknown'],
      ['C11 wrong-type', { usedPercent: 'wrong', resetsAt: now + 18_000, windowMinutes: 300 }, 'unknown'],
    ];
    for (const [id, value, expected] of vectors) {
      expect(codexPingWindowState(value as never, now), id).toBe(expected);
    }
  });

  it('L01..L21 match the Rust Claude table and strictly parse RFC3339', () => {
    const iso = (seconds: number) => new Date(seconds * 1000).toISOString();
    const session = (percentage: number, resetTime?: string) => ({ used: percentage, limit: 100, percentage, resetTime });
    const vectors: Array<[string, unknown, string]> = [
      ['L01 null-reset', session(0), 'closed'],
      ['L02 placeholder', session(0, iso(now + 18_000)), 'unknown'],
      ['L03 anchor', session(0, iso(now + 7_200)), 'open'],
      ['L04 used-positive', session(1), 'open'],
      ['L05 expired-reset', session(0, '1970-01-01T00:00:00Z'), 'unknown'],
      ['L06 negative-utilization', session(-1), 'unknown'],
      ['L07 non-finite-utilization', session(Number.NaN), 'unknown'],
      ['L08 non-rfc-english', session(0, 'October 5, 2026 12:00:00 GMT'), 'unknown'],
      ['L09 non-rfc-slashes', session(0, '2026/10/05 12:00:00'), 'unknown'],
      ['L10 invalid-date', session(0, 'not-a-date'), 'unknown'],
      ['L11 missing-session', undefined, 'unknown'],
      ['L12 threshold-minus-one', session(0, iso(now + 17_939)), 'open'],
      ['L13 threshold-equal', session(0, iso(now + 17_940)), 'unknown'],
      ['L14 threshold-plus-one', session(0, iso(now + 17_941)), 'unknown'],
      ['L15 lowercase-t-and-z', session(0, '2026-10-05t12:00:00z'), 'unknown'],
      ['L16 space-separator', session(0, '2026-10-05 12:00:00Z'), 'unknown'],
      ['L17 invalid-calendar-day', session(0, '2026-09-31T12:00:00Z'), 'unknown'],
      ['L18 non-leap-february-29', session(0, '2026-02-29T12:00:00Z'), 'unknown'],
      ['L19 leap-february-29', session(0, '2028-02-29T12:00:00Z'), 'open'],
      ['L20 hour-24', session(0, '2026-10-05T24:00:00Z'), 'unknown'],
      ['L21 leap-second', session(0, '2026-10-05T12:00:60Z'), 'unknown'],
    ];
    for (const [id, value, expected] of vectors) {
      const vectorNow = {
        'L17 invalid-calendar-day': Date.parse('2026-10-01T10:00:00Z') / 1000,
        'L18 non-leap-february-29': Date.parse('2026-03-01T10:00:00Z') / 1000,
        'L19 leap-february-29': Date.parse('2028-02-29T10:00:00Z') / 1000,
        'L20 hour-24': Date.parse('2026-10-05T22:00:00Z') / 1000,
      }[id] ?? now;
      expect(claudePingWindowState(value as never, vectorNow), id).toBe(expected);
    }
    expect(claudePingWindowState({ used: 0, limit: 100, percentage: 'wrong' } as never, now)).toBe('unknown');
    expect(parseRfc3339EpochSeconds('October 5, 2026 12:00:00 GMT')).toBeUndefined();
    expect(parseRfc3339EpochSeconds('2026/10/05 12:00:00')).toBeUndefined();
  });
});
