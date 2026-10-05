import { afterEach, describe, expect, it } from 'vitest';
import {
  CLAUDE_MENU_BAR_QUOTA_WINDOW_STORAGE_KEY,
  getSavedClaudeMenuBarQuotaWindow,
  getSavedMenuBarQuotaWindow,
  MENU_BAR_QUOTA_WINDOW_STORAGE_KEY,
  saveClaudeMenuBarQuotaWindow,
  saveMenuBarQuotaWindow,
} from '../src/services/codex_tray_window';
import { getCodexTrayUsedPercent, type CodexTrayAccountSnapshot } from '../src/services/provider_summary';

function installStorage(initial: Record<string, string> = {}) {
  const values = new Map(Object.entries(initial));
  (globalThis as Record<string, unknown>).localStorage = {
    clear: () => values.clear(),
    getItem: (key: string) => values.get(key) ?? null,
    key: (index: number) => Array.from(values.keys())[index] ?? null,
    get length() { return values.size; },
    removeItem: (key: string) => values.delete(key),
    setItem: (key: string, value: string) => values.set(key, value),
  };
  return values;
}

afterEach(() => {
  delete (globalThis as Record<string, unknown>).localStorage;
});

const accounts: CodexTrayAccountSnapshot[] = [
  {
    accountId: 'default', connected: true, freshness: 'fresh',
    ordinaryUsageAllowed: true,
    primary: { usedPercent: 18, windowMinutes: 300 },
    secondary: { usedPercent: 61, windowMinutes: 10_080 },
  },
  {
    accountId: 'work', connected: true, freshness: 'fresh',
    ordinaryUsageAllowed: true,
    primary: { usedPercent: 73, windowMinutes: 300 },
    secondary: { usedPercent: 42, windowMinutes: 10_080 },
  },
];

describe('Codex menu-bar quota window', () => {
  it('selects the maximum independently for weekly and five-hour windows', () => {
    expect(getCodexTrayUsedPercent(accounts, 'weekly')).toBe(61);
    expect(getCodexTrayUsedPercent(accounts, 'five_hour')).toBe(73);
  });

  it('excludes missing, disconnected, and invalid selected windows without fallback', () => {
    const invalid: CodexTrayAccountSnapshot[] = [
      { accountId: 'missing-weekly', connected: true, freshness: 'fresh', ordinaryUsageAllowed: true, primary: { usedPercent: 91, windowMinutes: 300 } },
      { accountId: 'offline', connected: false, freshness: 'fresh', secondary: { usedPercent: 80, windowMinutes: 10_080 } },
      { accountId: 'malformed', connected: true, freshness: 'fresh', ordinaryUsageAllowed: true, secondary: { usedPercent: Number.NaN, windowMinutes: 10_080 } },
    ];
    expect(getCodexTrayUsedPercent(invalid, 'weekly')).toBeNull();
    expect(getCodexTrayUsedPercent(invalid, 'five_hour')).toBe(91);
  });

  it('keeps connected quota observations independent from ordinary-usage permission', () => {
    const observed: CodexTrayAccountSnapshot[] = [
      // Sanitized provider fixture: a denied ordinary request with a real,
      // saturated five-hour quota is still a usable display observation.
      {
        accountId: 'saturated', connected: true, freshness: 'fresh', ordinaryUsageAllowed: false,
        primary: { usedPercent: 100, windowMinutes: 300 },
        secondary: { usedPercent: 0, windowMinutes: 10_080 },
      },
      { accountId: 'unknown-permission', connected: true, freshness: 'fresh', ordinaryUsageAllowed: null, secondary: { usedPercent: 98, windowMinutes: 10_080 } },
      { accountId: 'permitted', connected: true, freshness: 'fresh', ordinaryUsageAllowed: true, secondary: { usedPercent: 40, windowMinutes: 10_080 } },
    ];
    expect(getCodexTrayUsedPercent(observed, 'five_hour')).toBe(100);
    expect(getCodexTrayUsedPercent(observed, 'weekly')).toBe(98);
  });

  it.each([true, false, null])('keeps an over-limit 120%% five-hour quota for ordinaryUsageAllowed=%s on its own', (allowed) => {
    // One account per aggregation, so dropping any single permission state fails.
    const only: CodexTrayAccountSnapshot[] = [{
      accountId: 'only', connected: true, freshness: 'fresh', ordinaryUsageAllowed: allowed,
      primary: { usedPercent: 120, windowMinutes: 300 },
      secondary: { usedPercent: 0, windowMinutes: 10_080 },
    }];
    expect(getCodexTrayUsedPercent(only, 'five_hour')).toBe(120);
  });

  it('keeps absent selected-window usage unknown instead of converting it to zero', () => {
    const missing: CodexTrayAccountSnapshot[] = [
      { accountId: 'missing-primary', connected: true, freshness: 'fresh', ordinaryUsageAllowed: false, secondary: { usedPercent: 100, windowMinutes: 10_080 } },
      { accountId: 'null-permission', connected: true, freshness: 'fresh', ordinaryUsageAllowed: null, secondary: { usedPercent: 75, windowMinutes: 10_080 } },
    ];
    expect(getCodexTrayUsedPercent(missing, 'five_hour')).toBeNull();
  });

  it('preserves an explicit zero percent observation', () => {
    expect(getCodexTrayUsedPercent([
      {
        accountId: 'fresh-zero', connected: true, freshness: 'fresh', ordinaryUsageAllowed: true,
        primary: { usedPercent: 0, windowMinutes: 300 },
      },
    ], 'five_hour')).toBe(0);
  });

  it('does not infer quota recovery from a passed reset timestamp', () => {
    const pastReset = Math.floor(Date.now() / 1000) - 60;
    const futureReset = Math.floor(Date.now() / 1000) + 60;
    expect(getCodexTrayUsedPercent([
      {
        accountId: 'past-reset', connected: true, freshness: 'fresh',
        primary: { usedPercent: 100, windowMinutes: 300, resetsAt: pastReset },
      },
    ], 'five_hour')).toBe(100);
    expect(getCodexTrayUsedPercent([
      {
        accountId: 'future-reset', connected: true, freshness: 'fresh',
        primary: { usedPercent: 0, windowMinutes: 300, resetsAt: futureReset },
      },
    ], 'five_hour')).toBe(0);
  });

  it('aggregates only fresh accounts and returns null when all are stale or unavailable', () => {
    const mixed: CodexTrayAccountSnapshot[] = [
      { accountId: 'stale', connected: true, freshness: 'last-good-stale', secondary: { usedPercent: 99, windowMinutes: 10_080 } },
      { accountId: 'unavailable', connected: false, freshness: 'unavailable' },
      { accountId: 'fresh', connected: true, freshness: 'fresh', secondary: { usedPercent: 42, windowMinutes: 10_080 } },
    ];
    expect(getCodexTrayUsedPercent(mixed, 'weekly')).toBe(42);
    expect(getCodexTrayUsedPercent(mixed.slice(0, 2), 'weekly')).toBeNull();
  });

  it('defaults malformed persistence to weekly and saves validated changes', () => {
    const values = installStorage({ [MENU_BAR_QUOTA_WINDOW_STORAGE_KEY]: 'monthly' });
    expect(getSavedMenuBarQuotaWindow()).toBe('weekly');
    expect(saveMenuBarQuotaWindow('five_hour')).toBe(true);
    expect(values.get(MENU_BAR_QUOTA_WINDOW_STORAGE_KEY)).toBe('five_hour');
  });

  it('stores Claude and Codex window choices independently', () => {
    const values = installStorage({ [MENU_BAR_QUOTA_WINDOW_STORAGE_KEY]: 'five_hour' });

    expect(getSavedClaudeMenuBarQuotaWindow()).toBe('weekly');
    expect(saveClaudeMenuBarQuotaWindow('five_hour')).toBe(true);
    expect(getSavedClaudeMenuBarQuotaWindow()).toBe('five_hour');
    expect(values.get(MENU_BAR_QUOTA_WINDOW_STORAGE_KEY)).toBe('five_hour');

    expect(saveMenuBarQuotaWindow('weekly')).toBe(true);
    expect(getSavedMenuBarQuotaWindow()).toBe('weekly');
    expect(values.get(CLAUDE_MENU_BAR_QUOTA_WINDOW_STORAGE_KEY)).toBe('five_hour');
  });

  it('defaults an invalid Claude window choice to weekly', () => {
    installStorage({ [CLAUDE_MENU_BAR_QUOTA_WINDOW_STORAGE_KEY]: 'monthly' });

    expect(getSavedClaudeMenuBarQuotaWindow()).toBe('weekly');
  });
});
