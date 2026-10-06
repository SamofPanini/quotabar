import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, test, vi, type Mock } from 'vitest';
import { backend } from '../src/services/backend';
import { SERVICES } from '../src/services/service_meta';
import type { TrayServiceName } from '../src/services/tray_visibility';
import type { CodexProfilesResponse } from '../src/types/models';
import type { QuotaData } from '../src/types/models';
import SettingsView from '../src/components/SettingsView';
import { CLAUDE_MENU_BAR_QUOTA_WINDOW_STORAGE_KEY } from '../src/services/codex_tray_window';

vi.mock('../src/hooks/use_popover_window', () => ({
  usePopoverWindow: () => false,
}));

import App from '../src/App';

function memoryStorage(initial: Record<string, string>) {
  const values = new Map(Object.entries(initial));
  return {
    clear: () => values.clear(),
    getItem: (key: string) => values.get(key) ?? null,
    key: (index: number) => Array.from(values.keys())[index] ?? null,
    get length() {
      return values.size;
    },
    removeItem: (key: string) => {
      values.delete(key);
    },
    setItem: (key: string, value: string) => {
      values.set(key, value);
    },
  };
}

async function render_app(): Promise<ReactTestRenderer> {
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(createElement(App));
    await Promise.resolve();
    await Promise.resolve();
  });
  return renderer;
}

async function unmount(renderer: ReactTestRenderer): Promise<void> {
  await act(async () => renderer.unmount());
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

function visible_calls(update: ReturnType<typeof vi.spyOn>) {
  const visible = new Map<TrayServiceName, boolean>();
  for (const args of update.mock.calls) {
    const service = args[0] as TrayServiceName;
    visible.set(service, args[2] as boolean);
  }
  return visible;
}

function quota_with_percent(percentage: number): QuotaData {
  return {
    connected: true,
    weeklyTotal: { used: percentage, limit: 100, percentage },
  };
}

function claude_visible_calls(): unknown[][] {
  return (backend.updateTrayIcon as unknown as Mock).mock.calls.filter(
    (args) => args[0] === 'claude' && args[2] === true,
  );
}

beforeAll(() => {
  (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
});

beforeEach(() => {
  vi.useFakeTimers();
  (globalThis as Record<string, unknown>).localStorage = memoryStorage({
    'claude-tray-enabled': 'false',
    'codex-tray-enabled': 'true',
    'cursor-tray-enabled': 'false',
    'grok-tray-enabled': 'true',
    'antigravity-tray-enabled': 'false',
    'claude-quota-tray-cycle': 'false',
  });

  vi.spyOn(backend, 'getQuota').mockResolvedValue({ connected: true });
  vi.spyOn(backend, 'getCodexInfo').mockResolvedValue({ connected: true });
  vi.spyOn(backend, 'getCodexRateLimits').mockResolvedValue({ connected: true });
  vi.spyOn(backend, 'getCodexResetCredits').mockResolvedValue({
    connected: true,
    availableCount: 0,
    credits: [],
  });
  vi.spyOn(backend, 'getCodexWeeklyQuota').mockResolvedValue({});
  vi.spyOn(backend, 'getCodexProfiles').mockResolvedValue({ profiles: [], registryError: null } satisfies CodexProfilesResponse);
  vi.spyOn(backend, 'getCursorInfo').mockResolvedValue({ connected: true });
  vi.spyOn(backend, 'getGrokInfo').mockResolvedValue({ connected: true, percentage: 39, products: [] });
  vi.spyOn(backend, 'getAntigravityInfo').mockResolvedValue({ connected: false, status: 'pending' });
  vi.spyOn(backend, 'setDockVisibility').mockResolvedValue(undefined);
  vi.spyOn(backend, 'updateTrayIcon').mockResolvedValue(undefined);
});

afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  delete (globalThis as Record<string, unknown>).localStorage;
});

afterAll(() => {
  delete (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT;
});

describe('tray icon sync', () => {
  test('uses Claude session usage for the tray while the overview stays weekly', async () => {
    localStorage.setItem('claude-quota-tab', 'all');
    localStorage.setItem(CLAUDE_MENU_BAR_QUOTA_WINDOW_STORAGE_KEY, 'five_hour');
    vi.mocked(backend.getQuota).mockResolvedValue({
      connected: true,
      session: { used: 120, limit: 100, percentage: 120 },
      weeklyTotal: { used: 82, limit: 100, percentage: 82 },
    });

    const renderer = await render_app();
    await act(flush);

    const claudeUpdate = vi.mocked(backend.updateTrayIcon).mock.calls
      .filter((call) => call[0] === 'claude')
      .at(-1);
    expect(claudeUpdate?.[1]).toBe(120);
    expect(JSON.stringify(renderer.toJSON())).toContain('82% used');
    await unmount(renderer);
  });

  test('keeps Claude tray usage unknown when five-hour session data is absent', async () => {
    localStorage.setItem(CLAUDE_MENU_BAR_QUOTA_WINDOW_STORAGE_KEY, 'five_hour');
    vi.mocked(backend.getQuota).mockResolvedValue({
      connected: true,
      weeklyTotal: { used: 88, limit: 100, percentage: 88 },
    });

    const renderer = await render_app();
    await act(flush);

    const claudeUpdate = vi.mocked(backend.updateTrayIcon).mock.calls
      .filter((call) => call[0] === 'claude')
      .at(-1);
    expect(claudeUpdate?.[1]).toBeNull();
    await unmount(renderer);
  });

  test('keeps every enabled provider tray visible when cycle is off', async () => {
    const renderer = await render_app();
    const visible = visible_calls(backend.updateTrayIcon as unknown as ReturnType<typeof vi.spyOn>);

    expect(visible.get('codex')).toBe(true);
    expect(visible.get('grok')).toBe(true);
    expect(visible.get('claude')).toBe(false);
    expect(visible.get('cursor')).toBe(false);
    expect(visible.get('antigravity')).toBe(false);
    expect(SERVICES.every((service) => visible.has(service))).toBe(true);

    await unmount(renderer);
  });

  test('changes the Codex tray window locally without another acquisition call', async () => {
    vi.mocked(backend.getCodexRateLimits).mockResolvedValue({
      connected: true,
      ordinaryUsageAllowed: true,
      primary: { usedPercent: 18, windowMinutes: 300 },
      secondary: { usedPercent: 52, windowMinutes: 10_080 },
    });
    vi.mocked(backend.getCodexProfiles).mockResolvedValue({
      profiles: [{
        alias: 'Work', status: 'connected', availableResetCredits: 0,
        ordinaryUsageAllowed: true,
        primary: { usedPercent: 79, windowMinutes: 300 },
        secondary: { usedPercent: 31, windowMinutes: 10_080 },
      }],
      registryError: null,
    });
    const renderer = await render_app();
    await act(flush);
    const acquisitionCounts = {
      info: vi.mocked(backend.getCodexInfo).mock.calls.length,
      limits: vi.mocked(backend.getCodexRateLimits).mock.calls.length,
      profiles: vi.mocked(backend.getCodexProfiles).mock.calls.length,
      credits: vi.mocked(backend.getCodexResetCredits).mock.calls.length,
      weekly: vi.mocked(backend.getCodexWeeklyQuota).mock.calls.length,
    };
    vi.mocked(backend.updateTrayIcon).mockClear();

    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Open settings' }).props.onClick();
      await flush();
    });
    await act(async () => {
      renderer.root.findByType(SettingsView).props.onMenuBarQuotaWindowChange('five_hour');
      await flush();
    });

    expect(localStorage.getItem('menuBarQuotaWindow')).toBe('five_hour');
    expect(acquisitionCounts).toEqual({
      info: vi.mocked(backend.getCodexInfo).mock.calls.length,
      limits: vi.mocked(backend.getCodexRateLimits).mock.calls.length,
      profiles: vi.mocked(backend.getCodexProfiles).mock.calls.length,
      credits: vi.mocked(backend.getCodexResetCredits).mock.calls.length,
      weekly: vi.mocked(backend.getCodexWeeklyQuota).mock.calls.length,
    });
    const codexUpdate = vi.mocked(backend.updateTrayIcon).mock.calls.find((call) => call[0] === 'codex');
    expect(codexUpdate?.[1]).toBe(79);
    expect(codexUpdate?.[2]).toBe(true);
    await unmount(renderer);
  });

  test.each([100, 120])('projects a %s%% five-hour quota despite a blocked ordinary-usage result', async (usedPercent) => {
    localStorage.setItem('menuBarQuotaWindow', 'five_hour');
    vi.mocked(backend.getCodexRateLimits).mockResolvedValue({
      connected: true,
      ordinaryUsageAllowed: false,
      primary: { usedPercent, windowMinutes: 300 },
      secondary: { usedPercent: 52, windowMinutes: 10_080 },
    });
    const renderer = await render_app();
    await act(flush);

    const codexUpdate = vi.mocked(backend.updateTrayIcon).mock.calls
      .filter((call) => call[0] === 'codex')
      .at(-1);
    expect(codexUpdate?.[1]).toBe(usedPercent);
    expect(JSON.stringify(renderer.toJSON())).toContain(`${usedPercent}%`);
    const progress = renderer.root.findAll((node) => (
      node.props['aria-valuetext'] === `${usedPercent}% used`
    ));
    expect(progress).toHaveLength(1);
    expect(progress[0].props['aria-valuenow']).toBe(100);
    await unmount(renderer);
  });

  test('keeps the newest tray completion cached per service and retries rejected requests', async () => {
    (globalThis as Record<string, unknown>).localStorage = memoryStorage({
      'claude-tray-enabled': 'true',
      'codex-tray-enabled': 'false',
      'cursor-tray-enabled': 'false',
      'grok-tray-enabled': 'true',
      'antigravity-tray-enabled': 'false',
      'claude-quota-tray-cycle': 'false',
    });
    const hung = () => new Promise<never>(() => {});
    vi.spyOn(backend, 'getCodexInfo').mockImplementation(hung);
    vi.spyOn(backend, 'getCodexRateLimits').mockImplementation(hung);
    vi.spyOn(backend, 'getCodexResetCredits').mockImplementation(hung);
    vi.spyOn(backend, 'getCodexWeeklyQuota').mockImplementation(hung);
    vi.spyOn(backend, 'getCursorInfo').mockImplementation(hung);
    vi.spyOn(backend, 'getAntigravityInfo').mockImplementation(hung);

    let quotaPercent = 10;
    vi.spyOn(backend, 'getQuota').mockImplementation(async () => quota_with_percent(quotaPercent));
    vi.spyOn(console, 'error').mockImplementation(() => {});
    const inflight: Array<{ percentage: number; resolve(): void }> = [];
    let rejectThirty = true;
    vi.spyOn(backend, 'updateTrayIcon').mockImplementation((service, percentage, visible) => {
      if (service === 'grok') return Promise.resolve();
      if (service !== 'claude' || !visible || percentage == null) return Promise.resolve();
      if (percentage === 30 && rejectThirty) {
        rejectThirty = false;
        return Promise.reject(new Error('synthetic tray failure'));
      }
      return new Promise((resolve) => {
        inflight.push({ percentage, resolve: () => resolve(undefined) });
      });
    });

    const renderer = await render_app();
    await act(flush);
    expect(inflight.some((call) => call.percentage === 10)).toBe(true);
    expect(vi.mocked(backend.updateTrayIcon).mock.calls.some((call) => call[0] === 'grok')).toBe(true);

    quotaPercent = 20;
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Refresh current provider' }).props.onClick();
      await flush();
    });
    const newest = inflight.at(-1)!;
    expect(newest.percentage).toBe(20);
    await act(async () => {
      newest.resolve();
      await flush();
    });
    for (const call of inflight.filter((call) => call !== newest)) {
      await act(async () => {
        call.resolve();
        await flush();
      });
    }
    const callsAfterRace = claude_visible_calls().length;
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Refresh current provider' }).props.onClick();
      await flush();
    });
    expect(claude_visible_calls()).toHaveLength(callsAfterRace);

    quotaPercent = 30;
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Refresh current provider' }).props.onClick();
      await flush();
    });
    const callsAfterReject = claude_visible_calls().length;
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Refresh current provider' }).props.onClick();
      await flush();
    });
    expect(claude_visible_calls()).toHaveLength(callsAfterReject + 1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(claude_visible_calls().length).toBeGreaterThanOrEqual(callsAfterReject + 2);
    await unmount(renderer);
  });

  test('keeps skip-cache tuples independent when Claude and Grok requests complete out of order', async () => {
    (globalThis as Record<string, unknown>).localStorage = memoryStorage({
      'claude-tray-enabled': 'true',
      'codex-tray-enabled': 'false',
      'cursor-tray-enabled': 'false',
      'grok-tray-enabled': 'true',
      'antigravity-tray-enabled': 'false',
      'claude-quota-tray-cycle': 'false',
    });
    type PendingUpdate = { service: 'claude' | 'grok'; sequence: number; resolve(): void };
    const pending: PendingUpdate[] = [];
    let sequence = 0;
    vi.spyOn(backend, 'updateTrayIcon').mockImplementation((service) => {
      if (service !== 'claude' && service !== 'grok') return Promise.resolve();
      return new Promise((resolve) => {
        pending.push({ service, sequence: sequence++, resolve: () => resolve(undefined) });
      });
    });

    const renderer = await render_app();
    await act(flush);
    const latest = (service: 'claude' | 'grok') => pending.filter((call) => call.service === service).at(-1)!;
    const claude = latest('claude');
    const grok = latest('grok');
    const [later, earlier] = claude.sequence > grok.sequence ? [claude, grok] : [grok, claude];
    await act(async () => {
      later.resolve();
      await flush();
      earlier.resolve();
      await flush();
    });

    const visibleCalls = (service: 'claude' | 'grok') => vi.mocked(backend.updateTrayIcon).mock.calls.filter(
      (call) => call[0] === service && call[2] === true,
    );
    const beforeUnchanged = { claude: visibleCalls('claude').length, grok: visibleCalls('grok').length };
    // Change only a third service (Codex, tray hidden) so a non-force sync must
    // run while the Claude and Grok tuples stay identical.
    const codexCalls = () => vi.mocked(backend.updateTrayIcon).mock.calls.filter((call) => call[0] === 'codex');
    const codexBefore = codexCalls().length;
    vi.mocked(backend.getCodexRateLimits).mockResolvedValue({
      connected: true,
      secondary: { usedPercent: 55, windowMinutes: 10_080 },
    });
    await act(async () => {
      renderer.root.findByProps({ 'data-provider': 'codex' }).props.onClick();
      await flush();
    });
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Refresh current provider' }).props.onClick();
      await flush();
    });
    expect(codexCalls().length).toBeGreaterThan(codexBefore);
    expect(codexCalls().at(-1)?.[1]).toBe(55);
    expect(visibleCalls('claude')).toHaveLength(beforeUnchanged.claude);
    expect(visibleCalls('grok')).toHaveLength(beforeUnchanged.grok);

    vi.mocked(backend.getGrokInfo).mockResolvedValue({ connected: true, percentage: 40, products: [] });
    await act(async () => {
      renderer.root.findByProps({ 'data-provider': 'grok' }).props.onClick();
      await flush();
    });
    await act(async () => {
      renderer.root.findByProps({ 'aria-label': 'Refresh current provider' }).props.onClick();
      await flush();
    });
    expect(visibleCalls('claude')).toHaveLength(beforeUnchanged.claude);
    expect(visibleCalls('grok')).toHaveLength(beforeUnchanged.grok + 1);
    await unmount(renderer);
  });

  test('forces a tray update after a successful identical tuple has been cached', async () => {
    const renderer = await render_app();
    await act(flush);
    vi.mocked(backend.updateTrayIcon).mockClear();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
      await flush();
    });

    const grokCalls = vi.mocked(backend.updateTrayIcon).mock.calls.filter((call) => call[0] === 'grok');
    expect(grokCalls).toHaveLength(1);
    expect(grokCalls[0][3]).toBe(true);
    await unmount(renderer);
  });
});
