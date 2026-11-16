import { describe, expect, test, vi } from 'vitest';
import {
  AUTH_REFRESH_INTERVAL_MS,
  AUTO_REFRESH_INTERVAL_MS,
  BACKOFF_REFRESH_INTERVAL_MS,
  getClaudeRefreshIntervalMs,
  getClaudeTrayUsedPercent,
  getClaudeTrayQuotaForAccount,
  getClaudeTrayUsedPercentForAccount,
  getClaudeTrayUsedPercentForWindow,
  keepClaudeQuotaOnError,
} from '../src/App';
import { getSavedClaudeMenuBarAccount, saveClaudeMenuBarAccount } from '../src/services/app_state';
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

  test('includes the shared Fable quota in weekly bucket fallback', () => {
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

  test.each([Number.NaN, -1])('rejects invalid five-hour session percentages: %s', (percentage) => {
    expect(getClaudeTrayUsedPercentForWindow({
      connected: true,
      session: usage(percentage),
    }, 'five_hour')).toBeNull();
  });

  test('preserves over-limit five-hour session percentages', () => {
    expect(getClaudeTrayUsedPercentForWindow({
      connected: true,
      session: usage(120),
    }, 'five_hour')).toBe(120);
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

const profile = (alias: string, percentage: number | null, status: 'ok' | 'signedOut' | 'disabled' = 'ok') => ({
  alias,
  label: alias,
  status,
  account: null,
  quota: percentage === null ? null : { connected: true, weeklyTotal: usage(percentage) },
});

describe('Claude tray account selection', () => {
  const defaultQuota = { connected: true, weeklyTotal: usage(42) };

  test('claude_tray_account_default_unchanged', () => {
    expect(getClaudeTrayUsedPercentForAccount(defaultQuota, [profile('lucky', 81)], 'default', 'weekly'))
      .toBe(getClaudeTrayUsedPercentForWindow(defaultQuota, 'weekly'));
  });

  test('claude_tray_account_alias', () => {
    expect(getClaudeTrayUsedPercentForAccount(defaultQuota, [profile('lucky', 81)], 'lucky', 'weekly')).toBe(81);
  });

  test('claude_tray_account_missing_alias_falls_back', () => {
    expect(getClaudeTrayUsedPercentForAccount(defaultQuota, [], 'missing', 'weekly')).toBe(42);
  });

  test('claude_tray_account_signed_out_is_null', () => {
    expect(getClaudeTrayUsedPercentForAccount(defaultQuota, [profile('lucky', 81, 'signedOut')], 'lucky', 'weekly')).toBeNull();
    expect(getClaudeTrayUsedPercentForAccount(defaultQuota, [profile('lucky', 81, 'disabled')], 'lucky', 'weekly')).toBeNull();
  });

  test('claude_tray_account_highest', () => {
    const profiles = [profile('lucky', 81), profile('other', 65)];
    expect(getClaudeTrayUsedPercentForAccount(defaultQuota, profiles, 'highest', 'weekly')).toBe(81);
  });

  test('claude_tray_account_grayed_follows_selection', () => {
    const stale = { connected: true, weeklyTotal: usage(81), staleReason: 'expired' as const };
    const selected = getClaudeTrayQuotaForAccount(defaultQuota, [{ ...profile('lucky', 81), quota: stale }], 'lucky', 'weekly');
    expect(selected?.staleReason).toBe('expired');
    expect(getClaudeTrayQuotaForAccount(defaultQuota, [{ ...profile('lucky', 81), quota: stale }], 'default', 'weekly')?.staleReason).toBeUndefined();
  });

  test('Claude menu-bar account setting persists its selection', () => {
    const values = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
    });
    expect(getSavedClaudeMenuBarAccount()).toBe('default');
    expect(saveClaudeMenuBarAccount('lucky')).toBe(true);
    expect(getSavedClaudeMenuBarAccount()).toBe('lucky');
    vi.unstubAllGlobals();
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
