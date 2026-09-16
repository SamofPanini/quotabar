import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterEach, describe, expect, it, vi } from 'vitest';
import CodexPanel from '../src/components/CodexPanel';
import { backend } from '../src/services/backend';
import type {
  CodexProfileQuota,
  CodexProfilesResponse,
  CostDailySeries,
  CostOverview,
} from '../src/types/models';

const hiddenSections = { timeline: false, cost: false, trend: false, tips: false };

const profile = (alias: string, usedPercent = 12): CodexProfileQuota => ({
  alias,
  status: 'connected',
  planType: 'plus',
  primary: { usedPercent, windowMinutes: 300, resetsAt: 1_788_000_000 },
  secondary: { usedPercent: usedPercent + 10, windowMinutes: 10_080, resetsAt: 1_788_600_000 },
  availableResetCredits: 2,
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((next) => { resolve = next; });
  return { promise, resolve };
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

function mockDefaultCalls(): void {
  vi.spyOn(backend, 'getCodexInfo').mockResolvedValue({ connected: true, planType: 'plus' });
  vi.spyOn(backend, 'getCodexRateLimits').mockResolvedValue({
    connected: true,
    planType: 'plus',
    primary: { usedPercent: 8, windowMinutes: 300, resetsAt: 1_788_000_000 },
    secondary: { usedPercent: 20, windowMinutes: 10_080, resetsAt: 1_788_600_000 },
  });
  vi.spyOn(backend, 'getCodexResetCredits').mockResolvedValue({
    connected: true,
    availableCount: 1,
    credits: [],
  });
  vi.spyOn(backend, 'getCodexWeeklyQuota').mockResolvedValue({});
}

async function renderPanel(options: {
  profiles?: CodexProfilesResponse;
  onUsageChange?: (used: number | null) => void;
  manualRefreshNonce?: number;
  showCostSummary?: boolean;
  sections?: typeof hiddenSections;
} = {}): Promise<ReactTestRenderer> {
  mockDefaultCalls();
  vi.spyOn(backend, 'getCodexProfiles').mockResolvedValue(options.profiles ?? { profiles: [], registryError: null });
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(createElement(CodexPanel, {
      autoRefreshIntervalMs: 0,
      showCostSummary: options.showCostSummary ?? false,
      sections: options.sections ?? hiddenSections,
      onUsageChange: options.onUsageChange,
      manualRefreshNonce: options.manualRefreshNonce,
    }));
    await flush();
  });
  return renderer;
}

function emptyCostOverview(): CostOverview {
  return {
    source: 'codex', displayName: 'Codex', currency: 'USD',
    generatedAt: '2026-09-16T00:00:00Z', cached: false, ranges: [],
  };
}

function emptyCostDaily(): CostDailySeries {
  return {
    source: 'codex', currency: 'USD',
    generatedAt: '2026-09-16T00:00:00Z', cached: false, days: [],
  };
}

describe('Codex account tabs', () => {
  afterEach(() => vi.restoreAllMocks());

  it('keeps the default-only panel unchanged when the registry is missing', async () => {
    const renderer = await renderPanel();
    expect(renderer.root.findAllByProps({ role: 'tab' })).toHaveLength(0);
    expect(JSON.stringify(renderer.toJSON())).toContain('Connected');
    expect(backend.getCodexProfiles).toHaveBeenCalledTimes(1);
    await act(async () => renderer.unmount());
  });

  it('renders ordered public aliases and switches visible details without another IPC refresh', async () => {
    const onUsageChange = vi.fn();
    const workProfile = Object.assign(profile('Work', 14), {
      home: '/synthetic-private-home',
      email: 'synthetic@example.test',
      accountId: 'synthetic-account-id',
    }) as CodexProfileQuota;
    const renderer = await renderPanel({
      profiles: { profiles: [workProfile, profile('Personal', 31)], registryError: null },
      onUsageChange,
    });
    const tabs = renderer.root.findAllByProps({ role: 'tab' });
    expect(tabs.map((tab) => tab.children.join(''))).toEqual(['Default', 'Work', 'Personal']);
    expect(backend.getCodexProfiles).toHaveBeenCalledTimes(1);
    const callbackCount = onUsageChange.mock.calls.length;

    await act(async () => {
      tabs[1].props.onClick();
      await flush();
    });

    const text = JSON.stringify(renderer.toJSON());
    expect(text).toContain('Reset credits');
    expect(text).toContain('2');
    expect(text).toContain('ChatGPT Plus');
    expect(text).not.toContain('/synthetic-private-home');
    expect(text).not.toContain('synthetic@example.test');
    expect(text).not.toContain('synthetic-account-id');
    expect(backend.getCodexProfiles).toHaveBeenCalledTimes(1);
    expect(backend.getCodexInfo).toHaveBeenCalledTimes(1);
    expect(onUsageChange).toHaveBeenCalledTimes(callbackCount);
    await act(async () => renderer.unmount());
  });

  it('falls back to Default when a selected alias disappears on a later refresh', async () => {
    const renderer = await renderPanel({ profiles: { profiles: [profile('Work')], registryError: null } });
    await act(async () => {
      renderer.root.findAllByProps({ role: 'tab' })[1].props.onClick();
      await flush();
    });
    vi.mocked(backend.getCodexProfiles).mockResolvedValue({ profiles: [], registryError: null });
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        manualRefreshNonce: 1,
      }));
      await flush();
    });
    expect(renderer.root.findAllByProps({ role: 'tab' })).toHaveLength(0);
    expect(JSON.stringify(renderer.toJSON())).toContain('Connected');
    await act(async () => renderer.unmount());
  });

  it('keeps custom tabs reachable when the default account is offline', async () => {
    mockDefaultCalls();
    vi.mocked(backend.getCodexInfo).mockResolvedValue({ connected: false });
    vi.mocked(backend.getCodexRateLimits).mockResolvedValue({ connected: false });
    vi.mocked(backend.getCodexResetCredits).mockResolvedValue({
      connected: false,
      availableCount: 0,
      credits: [],
    });
    vi.spyOn(backend, 'getCodexProfiles').mockResolvedValue({ profiles: [profile('Work')], registryError: null });
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
      }));
      await flush();
    });
    await act(async () => {
      renderer.root.findAllByProps({ role: 'tab' })[1].props.onClick();
      await flush();
    });
    expect(JSON.stringify(renderer.toJSON())).toContain('Reset credits');
    await act(async () => renderer.unmount());
  });

  it('does not let an older profiles response overwrite a newer refresh', async () => {
    mockDefaultCalls();
    const older = deferred<CodexProfilesResponse>();
    vi.spyOn(backend, 'getCodexProfiles')
      .mockReturnValueOnce(older.promise)
      .mockResolvedValueOnce({ profiles: [profile('Newer')], registryError: null });
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
      }));
      await flush();
    });
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        manualRefreshNonce: 1,
      }));
      await flush();
    });
    await act(async () => {
      older.resolve({ profiles: [profile('Older')], registryError: null });
      await flush();
    });
    expect(renderer.root.findAllByProps({ role: 'tab' }).map((tab) => tab.children.join('')))
      .toEqual(['Default', 'Newer']);
    await act(async () => renderer.unmount());
  });

  it('does not remount default cost IPC when tabs round-trip through a custom account', async () => {
    vi.spyOn(backend, 'getCostOverview').mockResolvedValue(emptyCostOverview());
    vi.spyOn(backend, 'getCostDaily').mockResolvedValue(emptyCostDaily());
    const renderer = await renderPanel({
      profiles: { profiles: [profile('Work')], registryError: null },
      showCostSummary: true,
      sections: { ...hiddenSections, cost: true },
    });
    await act(async () => { await flush(); });

    const callCounts = () => ({
      info: vi.mocked(backend.getCodexInfo).mock.calls.length,
      limits: vi.mocked(backend.getCodexRateLimits).mock.calls.length,
      credits: vi.mocked(backend.getCodexResetCredits).mock.calls.length,
      weekly: vi.mocked(backend.getCodexWeeklyQuota).mock.calls.length,
      profiles: vi.mocked(backend.getCodexProfiles).mock.calls.length,
      costOverview: vi.mocked(backend.getCostOverview).mock.calls.length,
      costDaily: vi.mocked(backend.getCostDaily).mock.calls.length,
    });
    const initialCalls = callCounts();
    expect(initialCalls.costOverview).toBe(1);
    expect(initialCalls.costDaily).toBe(1);

    await act(async () => {
      renderer.root.findAllByProps({ role: 'tab' })[1].props.onClick();
      await flush();
    });
    await act(async () => {
      renderer.root.findAllByProps({ role: 'tab' })[0].props.onClick();
      await flush();
    });

    expect(callCounts()).toEqual(initialCalls);
    expect(JSON.stringify(renderer.toJSON())).toContain('API-equivalent usage');
    await act(async () => renderer.unmount());
  });
});
