import { afterEach, describe, expect, it } from 'vitest';
import {
  getSavedMenuBarQuotaWindow,
  MENU_BAR_QUOTA_WINDOW_STORAGE_KEY,
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
    accountId: 'default', connected: true,
    ordinaryUsageAllowed: true,
    primary: { usedPercent: 18, windowMinutes: 300 },
    secondary: { usedPercent: 61, windowMinutes: 10_080 },
  },
  {
    accountId: 'work', connected: true,
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
      { accountId: 'missing-weekly', connected: true, ordinaryUsageAllowed: true, primary: { usedPercent: 91, windowMinutes: 300 } },
      { accountId: 'offline', connected: false, secondary: { usedPercent: 80, windowMinutes: 10_080 } },
      { accountId: 'malformed', connected: true, ordinaryUsageAllowed: true, secondary: { usedPercent: Number.NaN, windowMinutes: 10_080 } },
    ];
    expect(getCodexTrayUsedPercent(invalid, 'weekly')).toBeNull();
    expect(getCodexTrayUsedPercent(invalid, 'five_hour')).toBe(91);
  });

  it('uses only accounts explicitly permitted for ordinary usage', () => {
    const unavailable: CodexTrayAccountSnapshot[] = [
      { accountId: 'blocked', connected: true, ordinaryUsageAllowed: false, secondary: { usedPercent: 99, windowMinutes: 10_080 } },
      { accountId: 'unknown', connected: true, ordinaryUsageAllowed: null, secondary: { usedPercent: 98, windowMinutes: 10_080 } },
      { accountId: 'permitted', connected: true, ordinaryUsageAllowed: true, secondary: { usedPercent: 40, windowMinutes: 10_080 } },
    ];
    expect(getCodexTrayUsedPercent(unavailable, 'weekly')).toBe(40);
    expect(getCodexTrayUsedPercent(unavailable.slice(0, 2), 'weekly')).toBeNull();
  });

  it('defaults malformed persistence to weekly and saves validated changes', () => {
    const values = installStorage({ [MENU_BAR_QUOTA_WINDOW_STORAGE_KEY]: 'monthly' });
    expect(getSavedMenuBarQuotaWindow()).toBe('weekly');
    expect(saveMenuBarQuotaWindow('five_hour')).toBe(true);
    expect(values.get(MENU_BAR_QUOTA_WINDOW_STORAGE_KEY)).toBe('five_hour');
  });
});
