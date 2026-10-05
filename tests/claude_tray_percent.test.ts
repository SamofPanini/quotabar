import { describe, expect, test } from 'vitest';
import {
  AUTH_REFRESH_INTERVAL_MS,
  AUTO_REFRESH_INTERVAL_MS,
  BACKOFF_REFRESH_INTERVAL_MS,
  getClaudeRefreshIntervalMs,
  getClaudeTrayUsedPercent,
  getClaudeTrayUsedPercentForWindow,
  keepClaudeQuotaOnError,
} from '../src/App';
import type { QuotaData, UsageInfo } from '../src/types/models';

const usage = (percentage: number): UsageInfo => ({
  used: percentage,
  limit: 100,
  percentage,
});

describe('getClaudeTrayUsedPercent', () => {
  test('uses weekly total before individual weekly buckets', () => {
    expect(getClaudeTrayUsedPercent({
      connected: true,
      weeklyTotal: usage(42),
      weeklyDesign: usage(91),
      weeklyFable5: usage(96),
    })).toBe(42);
  });

  test('includes Claude Design in weekly bucket fallback', () => {
    expect(getClaudeTrayUsedPercent({
      connected: true,
      session: usage(12),
      weeklyOpus: usage(36),
      weeklyDesign: usage(84),
    })).toBe(84);
  });

  test('includes Fable 5 in weekly bucket fallback', () => {
    expect(getClaudeTrayUsedPercent({
      connected: true,
      session: usage(12),
      weeklyOpus: usage(36),
      weeklyFable5: usage(87),
    })).toBe(87);
  });

  test('falls back to session usage when weekly buckets are missing', () => {
    expect(getClaudeTrayUsedPercent({
      connected: true,
      session: usage(27),
    })).toBe(27);
  });

  test('returns null when no quota window exists', () => {
    const quota: QuotaData = { connected: true };

    expect(getClaudeTrayUsedPercent(null)).toBeNull();
    expect(getClaudeTrayUsedPercent(quota)).toBeNull();
  });
});

describe('getClaudeTrayUsedPercentForWindow', () => {
  test.each([
    ['weekly total', { connected: true, weeklyTotal: usage(42), weeklyDesign: usage(91) }],
    ['weekly buckets', { connected: true, weeklyOpus: usage(36), weeklyDesign: usage(84) }],
    ['session fallback', { connected: true, session: usage(27) }],
    ['no windows', { connected: true }],
    ['null quota', null],
  ] as const)('weekly matches the existing Claude value for %s', (_name, quota) => {
    expect(getClaudeTrayUsedPercentForWindow(quota, 'weekly')).toBe(getClaudeTrayUsedPercent(quota));
  });

  test('returns only the session value for five-hour mode', () => {
    expect(getClaudeTrayUsedPercentForWindow({
      connected: true,
      session: usage(31),
      weeklyTotal: usage(88),
    }, 'five_hour')).toBe(31);
  });

  test('does not fall back to weekly data when five-hour session data is absent', () => {
    expect(getClaudeTrayUsedPercentForWindow({
      connected: true,
      weeklyTotal: usage(88),
    }, 'five_hour')).toBeNull();
  });

  test.each([Number.NaN, -1, 101])('rejects invalid five-hour session percentages: %s', (percentage) => {
    expect(getClaudeTrayUsedPercentForWindow({
      connected: true,
      session: usage(percentage),
    }, 'five_hour')).toBeNull();
  });

  test('keeps showing retained quota while disconnected (429 backoff), like the weekly helper', () => {
    const quota = { connected: false, weeklyTotal: usage(42), session: usage(31) };
    expect(getClaudeTrayUsedPercentForWindow(quota, 'weekly')).toBe(getClaudeTrayUsedPercent(quota));
    expect(getClaudeTrayUsedPercentForWindow(quota, 'weekly')).toBe(42);
    expect(getClaudeTrayUsedPercentForWindow(quota, 'five_hour')).toBe(31);
  });

  test('returns null without quota in either mode', () => {
    expect(getClaudeTrayUsedPercentForWindow(null, 'weekly')).toBeNull();
    expect(getClaudeTrayUsedPercentForWindow(null, 'five_hour')).toBeNull();
  });
});

describe('getClaudeRefreshIntervalMs', () => {
  test('uses normal polling when Claude quota succeeds', () => {
    expect(getClaudeRefreshIntervalMs(null)).toBe(AUTO_REFRESH_INTERVAL_MS);
  });

  test('backs off briefly for rate limits', () => {
    expect(getClaudeRefreshIntervalMs('API error: 429 Too Many Requests')).toBe(
      BACKOFF_REFRESH_INTERVAL_MS,
    );
  });

  test('backs off to hourly polling for Claude auth failures', () => {
    expect(getClaudeRefreshIntervalMs(
      'Claude OAuth token expired or invalid. Please re-login to Claude Code, then click Refresh.',
    )).toBe(AUTH_REFRESH_INTERVAL_MS);
    expect(getClaudeRefreshIntervalMs('API error: 401 Unauthorized')).toBe(AUTH_REFRESH_INTERVAL_MS);
  });
});

describe('keepClaudeQuotaOnError', () => {
  test('keeps connected stale snapshots even when an error is present', () => {
    expect(keepClaudeQuotaOnError({
      connected: true,
      error: 'Network error: connection reset',
    })).toBe(true);
  });

  test('keeps prior quota for 429 even when disconnected', () => {
    expect(keepClaudeQuotaOnError({
      connected: false,
      error: 'API error: 429 Too Many Requests',
    })).toBe(true);
  });

  test('clears quota for disconnected non-429 errors', () => {
    expect(keepClaudeQuotaOnError({
      connected: false,
      error: 'API error: 401 Unauthorized',
    })).toBe(false);
  });
});
