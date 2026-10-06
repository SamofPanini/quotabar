import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterEach, describe, expect, it, vi } from 'vitest';
import CodexPanel from '../src/components/CodexPanel';
import { backend } from '../src/services/backend';
import type {
  CodexProfileQuota,
  CodexProfilesResponse,
  CodexRateLimits,
  CostDailySeries,
  CostOverview,
} from '../src/types/models';
import { getCodexTrayUsedPercent, type CodexTrayAccountSnapshot } from '../src/services/provider_summary';

const hiddenSections = { timeline: false, cost: false, trend: false, tips: false };

const profile = (
  alias: string,
  usedPercent = 12,
  ordinaryUsageAllowed: boolean | null | undefined = true,
): CodexProfileQuota => ({
  alias,
  status: 'connected',
  planType: 'plus',
  primary: { usedPercent, windowMinutes: 300, resetsAt: 1_788_000_000 },
  secondary: { usedPercent: usedPercent + 10, windowMinutes: 10_080, resetsAt: 1_788_600_000 },
  availableResetCredits: 2,
  ordinaryUsageAllowed,
});

function deferred<T>() {
  let reject!: (reason: unknown) => void;
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((next, fail) => {
    resolve = next;
    reject = fail;
  });
  return { promise, reject, resolve };
}

async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

function mockDefaultCalls(limits?: CodexRateLimits): void {
  vi.spyOn(backend, 'getCodexInfo').mockResolvedValue({ connected: true, planType: 'plus' });
  vi.spyOn(backend, 'getCodexRateLimits').mockResolvedValue(limits ?? {
    connected: true,
    planType: 'plus',
    ordinaryUsageAllowed: true,
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
  onTrayQuotaSnapshotsChange?: (snapshots: CodexTrayAccountSnapshot[]) => void;
  manualRefreshNonce?: number;
  defaultRateLimits?: CodexRateLimits;
  showCostSummary?: boolean;
  sections?: typeof hiddenSections;
} = {}): Promise<ReactTestRenderer> {
  mockDefaultCalls(options.defaultRateLimits);
  vi.spyOn(backend, 'getCodexProfiles').mockResolvedValue(options.profiles ?? {
    profiles: [],
    registryError: null,
    registryProvenance: 'none',
  });
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(createElement(CodexPanel, {
      autoRefreshIntervalMs: 0,
      showCostSummary: options.showCostSummary ?? false,
      sections: options.sections ?? hiddenSections,
      onUsageChange: options.onUsageChange,
      onTrayQuotaSnapshotsChange: options.onTrayQuotaSnapshotsChange,
      manualRefreshNonce: options.manualRefreshNonce,
    }));
    await flush();
  });
  return renderer;
}

async function mountPanel(options: {
  onTrayQuotaSnapshotsChange?: (snapshots: CodexTrayAccountSnapshot[]) => void;
} = {}): Promise<ReactTestRenderer> {
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(createElement(CodexPanel, {
      autoRefreshIntervalMs: 0,
      showCostSummary: false,
      sections: hiddenSections,
      onTrayQuotaSnapshotsChange: options.onTrayQuotaSnapshotsChange,
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

  it('shows safe registry state for primary, legacy, and no-registry responses', async () => {
    for (const [registryProvenance, expected] of [
      ['primary', 'Custom profile registry loaded.'],
      ['legacy', 'Using legacy custom profile registry.'],
      ['none', 'No custom profile registry configured.'],
    ] as const) {
      const renderer = await renderPanel({
        profiles: { profiles: [], registryError: null, registryProvenance },
      });
      expect(JSON.stringify(renderer.toJSON())).toContain(expected);
      await act(async () => renderer.unmount());
    }
  });

  it('keeps the default quota visible when the custom registry is malformed', async () => {
    const renderer = await renderPanel({
      profiles: {
        profiles: [],
        registryError: 'primary registry parse failed at /private/synthetic',
        registryProvenance: 'primary',
      },
    });
    const text = JSON.stringify(renderer.toJSON());
    expect(text).toContain('Connected');
    expect(text).toContain('Custom profile registry unavailable.');
    expect(text).not.toContain('primary registry parse failed');
    expect(text).not.toContain('/private/synthetic');
    await act(async () => renderer.unmount());
  });

  it('keeps a safe loading failure visible when the profile IPC rejects', async () => {
    mockDefaultCalls();
    vi.spyOn(backend, 'getCodexProfiles').mockRejectedValue(new Error('/private/raw registry failure'));
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
      }));
      await flush();
    });
    const text = JSON.stringify(renderer.toJSON());
    expect(text).toContain('Connected');
    expect(text).toContain('Custom profile registry unavailable.');
    expect(text).not.toContain('/private/raw registry failure');
    await act(async () => renderer.unmount());
  });

  it('clears a prior registry error after a successful refresh', async () => {
    mockDefaultCalls();
    vi.spyOn(backend, 'getCodexProfiles')
      .mockResolvedValueOnce({
        profiles: [],
        registryError: 'malformed primary',
        registryProvenance: 'primary',
      })
      .mockResolvedValueOnce({
        profiles: [],
        registryError: null,
        registryProvenance: 'none',
      });
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
      }));
      await flush();
    });
    expect(JSON.stringify(renderer.toJSON())).toContain('Custom profile registry unavailable.');
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        manualRefreshNonce: 1,
        showCostSummary: false,
        sections: hiddenSections,
      }));
      await flush();
    });
    const text = JSON.stringify(renderer.toJSON());
    expect(text).toContain('No custom profile registry configured.');
    expect(text).not.toContain('Custom profile registry unavailable.');
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

  it('keeps the Default credit balance isolated from custom account tabs', async () => {
    const renderer = await renderPanel({
      defaultRateLimits: {
        connected: true,
        primary: { usedPercent: 8, windowMinutes: 300, resetsAt: 1_788_000_000 },
        secondary: { usedPercent: 20, windowMinutes: 10_080, resetsAt: 1_788_600_000 },
        credits: { hasCredits: true, unlimited: false, balance: '1234.5' },
      },
      profiles: { profiles: [profile('Work', 14)], registryError: null },
    });
    expect(JSON.stringify(renderer.toJSON())).toContain('1,234.5');

    await act(async () => {
      renderer.root.findAllByProps({ role: 'tab' })[1].props.onClick();
      await flush();
    });
    expect(JSON.stringify(renderer.toJSON())).not.toContain('1,234.5');

    await act(async () => {
      renderer.root.findAllByProps({ role: 'tab' })[0].props.onClick();
      await flush();
    });
    expect(JSON.stringify(renderer.toJSON())).toContain('1,234.5');
    await act(async () => renderer.unmount());
  });

  it('keeps conflicting ordinary-usage labels and neutral meters bound to their selected tab', async () => {
    const unknownProfile = profile('Unknown', 44);
    delete unknownProfile.ordinaryUsageAllowed;
    const renderer = await renderPanel({
      defaultRateLimits: {
        connected: true,
        planType: 'plus',
        ordinaryUsageAllowed: true,
        primary: { usedPercent: 100, windowMinutes: 300, resetsAt: 1_788_000_000 },
        secondary: { usedPercent: 90, windowMinutes: 10_080, resetsAt: 1_788_600_000 },
      },
      profiles: {
        profiles: [
          profile('Blocked', 0, false),
          unknownProfile,
        ],
        registryError: null,
      },
    });
    const tabText = () => JSON.stringify(renderer.toJSON());
    const meterTexts = () => renderer.root.findAllByProps({ role: 'progressbar' })
      .map((meter) => meter.props['aria-valuetext']);
    const permissionLabels = () => renderer.root.findAllByProps({ className: 'codex-updated' })
      .map((node) => node.children.join(''))
      .filter((text) => [
        'Ordinary usage permitted',
        'Ordinary usage blocked',
        'Availability unknown',
      ].includes(text));
    const expectSelectedTab = (label: string, meters: string[]) => {
      expect(permissionLabels()).toEqual([label]);
      expect(meterTexts()).toEqual(meters);
      expect(tabText()).not.toContain('Weekly exhausted');
      expect(tabText()).not.toContain('Weekly is used up.');
      expect(tabText()).not.toContain('Resets after ordinary usage is restored');
      expect(tabText()).not.toContain('Codex weekly is at 100%.');
    };
    const clickTab = async (index: number) => {
      await act(async () => {
        renderer.root.findAllByProps({ role: 'tab' })[index].props.onClick();
        await flush();
      });
    };

    expectSelectedTab('Ordinary usage permitted', ['100% used', '90% used']);

    await clickTab(1);
    expectSelectedTab('Ordinary usage blocked', ['0% used', '10% used']);

    await clickTab(2);
    expectSelectedTab('Availability unknown', ['44% used', '54% used']);

    await clickTab(0);
    expectSelectedTab('Ordinary usage permitted', ['100% used', '90% used']);

    await clickTab(1);
    expectSelectedTab('Ordinary usage blocked', ['0% used', '10% used']);
    await act(async () => renderer.unmount());
  });

  it('publishes default and custom snapshots once per completed refresh, independent of tab selection', async () => {
    const onTrayQuotaSnapshotsChange = vi.fn();
    const renderer = await renderPanel({
      profiles: { profiles: [profile('Work', 35)], registryError: null },
      onTrayQuotaSnapshotsChange,
    });
    expect(onTrayQuotaSnapshotsChange).toHaveBeenCalledWith([
      expect.objectContaining({ accountId: 'default', connected: true }),
      expect.objectContaining({ accountId: 'Work', connected: true }),
    ]);
    const callbackCount = onTrayQuotaSnapshotsChange.mock.calls.length;

    await act(async () => {
      renderer.root.findAllByProps({ role: 'tab' })[1].props.onClick();
      await flush();
    });

    expect(onTrayQuotaSnapshotsChange).toHaveBeenCalledTimes(callbackCount);
    await act(async () => renderer.unmount());
  });

  it('marks a prior Default quota stale while a healthy custom account remains fresh for the tray', async () => {
    mockDefaultCalls();
    vi.spyOn(backend, 'getCodexProfiles')
      .mockResolvedValueOnce({ profiles: [profile('B', 20)], registryError: null })
      .mockResolvedValueOnce({ profiles: [profile('B', 80)], registryError: null });
    const onTrayQuotaSnapshotsChange = vi.fn();
    const renderer = await mountPanel({ onTrayQuotaSnapshotsChange });
    vi.mocked(backend.getCodexRateLimits).mockRejectedValueOnce(new Error('transport failure'));
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0, manualRefreshNonce: 1, showCostSummary: false,
        sections: hiddenSections, onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    const snapshots = onTrayQuotaSnapshotsChange.mock.calls.at(-1)![0] as CodexTrayAccountSnapshot[];
    expect(snapshots).toEqual([
      expect.objectContaining({ accountId: 'default', freshness: 'unavailable' }),
      expect.objectContaining({ accountId: 'B', freshness: 'fresh' }),
    ]);
    expect(getCodexTrayUsedPercent(snapshots, 'weekly')).toBe(90);
    expect(getCodexTrayUsedPercent(snapshots, 'weekly')).not.toBe(20);
    expect(JSON.stringify(renderer.toJSON())).toContain('Stale data');
    await act(async () => renderer.unmount());
  });

  it('publishes initial Default unavailable with a healthy custom account', async () => {
    mockDefaultCalls();
    vi.mocked(backend.getCodexRateLimits).mockRejectedValueOnce(new Error('transport failure'));
    vi.spyOn(backend, 'getCodexProfiles').mockResolvedValue({ profiles: [profile('B', 60)], registryError: null });
    const onTrayQuotaSnapshotsChange = vi.fn();
    const renderer = await mountPanel({ onTrayQuotaSnapshotsChange });
    const snapshots = onTrayQuotaSnapshotsChange.mock.calls.at(-1)![0] as CodexTrayAccountSnapshot[];
    expect(snapshots.map((snapshot) => [snapshot.accountId, snapshot.freshness])).toEqual([
      ['default', 'unavailable'], ['B', 'fresh'],
    ]);
    expect(getCodexTrayUsedPercent(snapshots, 'weekly')).toBe(70);
    expect(JSON.stringify(renderer.toJSON())).toContain('Quota unavailable');
    await act(async () => renderer.unmount());
  });

  it('discards an older delayed Default success after a newer Default failure', async () => {
    mockDefaultCalls();
    const olderLimits = deferred<CodexRateLimits>();
    vi.mocked(backend.getCodexRateLimits)
      .mockReturnValueOnce(olderLimits.promise)
      .mockRejectedValueOnce(new Error('newer failure'));
    vi.spyOn(backend, 'getCodexProfiles')
      .mockResolvedValueOnce({ profiles: [profile('Old')], registryError: null })
      .mockResolvedValueOnce({ profiles: [profile('B', 70)], registryError: null });
    const onTrayQuotaSnapshotsChange = vi.fn();
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0, showCostSummary: false, sections: hiddenSections, onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0, manualRefreshNonce: 1, showCostSummary: false, sections: hiddenSections, onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    await act(async () => {
      olderLimits.resolve({ connected: true, primary: { usedPercent: 99, windowMinutes: 300 } });
      await flush();
    });
    const published = onTrayQuotaSnapshotsChange.mock.calls.map(([snapshots]) => snapshots as CodexTrayAccountSnapshot[]);
    expect(published).toHaveLength(1);
    expect(published[0]).toEqual([
      expect.objectContaining({ accountId: 'default', freshness: 'unavailable' }),
      expect.objectContaining({ accountId: 'B', freshness: 'fresh' }),
    ]);
    await act(async () => renderer.unmount());
  });

  it('discards an older delayed Default failure after a newer Default success', async () => {
    mockDefaultCalls();
    const olderLimits = deferred<CodexRateLimits>();
    vi.mocked(backend.getCodexRateLimits)
      .mockReturnValueOnce(olderLimits.promise)
      .mockResolvedValueOnce({ connected: true, primary: { usedPercent: 40, windowMinutes: 300 }, secondary: { usedPercent: 50, windowMinutes: 10_080 } });
    vi.spyOn(backend, 'getCodexProfiles')
      .mockResolvedValueOnce({ profiles: [profile('Old')], registryError: null })
      .mockResolvedValueOnce({ profiles: [profile('B', 60)], registryError: null });
    const onTrayQuotaSnapshotsChange = vi.fn();
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0, showCostSummary: false, sections: hiddenSections, onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0, manualRefreshNonce: 1, showCostSummary: false, sections: hiddenSections, onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    await act(async () => {
      olderLimits.reject(new Error('older failure'));
      await flush();
    });
    expect(onTrayQuotaSnapshotsChange).toHaveBeenCalledTimes(1);
    expect(onTrayQuotaSnapshotsChange).toHaveBeenLastCalledWith([
      expect.objectContaining({ accountId: 'default', freshness: 'fresh' }),
      expect.objectContaining({ accountId: 'B', freshness: 'fresh' }),
    ]);
    const text = JSON.stringify(renderer.toJSON());
    expect(text).not.toContain('Quota unavailable');
    expect(text).not.toContain('Stale data');
    await act(async () => renderer.unmount());
  });

  it('keeps the selected healthy custom tab stable while an inactive profile is unavailable', async () => {
    const unavailable = { ...profile('B', 99), status: 'error' as const, primary: undefined, secondary: undefined };
    const onTrayQuotaSnapshotsChange = vi.fn();
    const renderer = await renderPanel({
      profiles: { profiles: [profile('A', 31), unavailable], registryError: null },
      onTrayQuotaSnapshotsChange,
    });
    await act(async () => {
      renderer.root.findAllByProps({ role: 'tab' })[1].props.onClick();
      await flush();
    });
    expect(renderer.root.findAllByProps({ role: 'tab' }).map((tab) => tab.props['aria-selected'])).toEqual([false, true, false]);
    expect(JSON.stringify(renderer.toJSON())).toContain('31% used');
    const snapshots = onTrayQuotaSnapshotsChange.mock.calls.at(-1)![0] as CodexTrayAccountSnapshot[];
    expect(snapshots).toEqual(expect.arrayContaining([
      expect.objectContaining({ accountId: 'A', freshness: 'fresh' }),
      expect.objectContaining({ accountId: 'B', freshness: 'unavailable' }),
    ]));
    expect(getCodexTrayUsedPercent(snapshots, 'weekly')).toBe(41);
    await act(async () => renderer.unmount());
  });

  it('returns an unknown tray value when every account is unavailable and atomically recovers on success', async () => {
    mockDefaultCalls();
    vi.mocked(backend.getCodexRateLimits)
      .mockRejectedValueOnce(new Error('failure'))
      .mockResolvedValueOnce({ connected: true, secondary: { usedPercent: 55, windowMinutes: 10_080 } });
    vi.spyOn(backend, 'getCodexProfiles')
      .mockResolvedValueOnce({ profiles: [{ ...profile('B'), status: 'offline', primary: undefined, secondary: undefined }], registryError: null })
      .mockResolvedValueOnce({ profiles: [profile('B', 65)], registryError: null });
    const onTrayQuotaSnapshotsChange = vi.fn();
    const renderer = await mountPanel({ onTrayQuotaSnapshotsChange });
    const unavailable = onTrayQuotaSnapshotsChange.mock.calls.at(-1)![0] as CodexTrayAccountSnapshot[];
    expect(getCodexTrayUsedPercent(unavailable, 'weekly')).toBeNull();
    expect(unavailable).not.toHaveLength(0);
    expect(unavailable.every((snapshot) => snapshot.freshness !== 'fresh')).toBe(true);
    const callsBeforeRecovery = onTrayQuotaSnapshotsChange.mock.calls.length;
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0, manualRefreshNonce: 1, showCostSummary: false, sections: hiddenSections, onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    const recovered = onTrayQuotaSnapshotsChange.mock.calls.at(-1)![0] as CodexTrayAccountSnapshot[];
    expect(onTrayQuotaSnapshotsChange).toHaveBeenCalledTimes(callsBeforeRecovery + 1);
    expect(recovered.every((snapshot) => snapshot.freshness === 'fresh')).toBe(true);
    expect(getCodexTrayUsedPercent(recovered, 'weekly')).toBe(75);
    await act(async () => renderer.unmount());
  });

  it('displays a fulfilled reset-credit error without replacing it with reject copy', async () => {
    mockDefaultCalls();
    vi.mocked(backend.getCodexResetCredits).mockResolvedValue({
      connected: true,
      availableCount: 0,
      credits: [],
      error: 'Reset credit lookup failed',
    });
    const renderer = await mountPanel();
    const text = JSON.stringify(renderer.toJSON());
    expect(text).toContain('Reset credit lookup failed');
    expect(text).not.toContain('Reset credits unavailable');
    await act(async () => renderer.unmount());
  });

  it('keeps quota fresh when only account-info or reset-credits IPC rejects', async () => {
    mockDefaultCalls();
    vi.mocked(backend.getCodexInfo).mockRejectedValueOnce(new Error('raw info failure'));
    vi.mocked(backend.getCodexResetCredits).mockRejectedValueOnce(new Error('raw credits failure'));
    vi.spyOn(backend, 'getCodexProfiles').mockResolvedValue({ profiles: [profile('B', 70)], registryError: null });
    const onTrayQuotaSnapshotsChange = vi.fn();
    const renderer = await mountPanel({ onTrayQuotaSnapshotsChange });
    const snapshots = onTrayQuotaSnapshotsChange.mock.calls.at(-1)![0] as CodexTrayAccountSnapshot[];
    expect(snapshots.every((snapshot) => snapshot.freshness === 'fresh')).toBe(true);
    expect(getCodexTrayUsedPercent(snapshots, 'weekly')).toBe(80);
    const text = JSON.stringify(renderer.toJSON());
    expect(text).toContain('Account info unavailable');
    expect(text).toContain('Reset credits unavailable');
    expect(text).not.toContain('raw info failure');
    expect(text).not.toContain('raw credits failure');
    await act(async () => renderer.unmount());
  });

  it('retains custom tabs as stale when the registry transport rejects', async () => {
    mockDefaultCalls();
    vi.spyOn(backend, 'getCodexProfiles')
      .mockResolvedValueOnce({ profiles: [profile('B', 70)], registryError: null })
      .mockRejectedValueOnce(new Error('/private/raw registry failure'));
    const onTrayQuotaSnapshotsChange = vi.fn();
    const renderer = await mountPanel({ onTrayQuotaSnapshotsChange });
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0, manualRefreshNonce: 1, showCostSummary: false, sections: hiddenSections, onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    const snapshots = onTrayQuotaSnapshotsChange.mock.calls.at(-1)![0] as CodexTrayAccountSnapshot[];
    expect(snapshots).toEqual([
      expect.objectContaining({ accountId: 'default', freshness: 'fresh' }),
      expect.objectContaining({ accountId: 'B', freshness: 'last-good-stale' }),
    ]);
    expect(getCodexTrayUsedPercent(snapshots, 'weekly')).toBe(20);
    expect(renderer.root.findAllByProps({ role: 'tab' }).map((tab) => tab.children.join(''))).toEqual(['Default', 'B']);
    expect(JSON.stringify(renderer.toJSON())).toContain('Custom profile registry unavailable.');
    await act(async () => renderer.unmount());
  });

  it('renders fixed safe diagnostic copy and falls back safely for unknown or missing codes', async () => {
    const messages = {
      invalid_row: 'This profile entry is invalid.',
      invalid_alias: 'This profile alias is invalid or duplicated.',
      invalid_home_path: 'This profile home must be an absolute path without .. components.',
      invalid_home: 'This profile home is unavailable or is not a directory.',
      default_home_conflict: 'This profile home is already the Default account.',
      duplicate_home: 'Another custom profile already uses this home.',
    };
    const unavailable = (diagnosticCode?: string): CodexProfileQuota => ({
      ...profile('Broken'), status: 'error', primary: undefined, secondary: undefined, diagnosticCode,
    });

    for (const [diagnosticCode, message] of Object.entries(messages)) {
      const renderer = await renderPanel({
        profiles: { profiles: [unavailable(diagnosticCode)], registryError: 'registry-only-detail' },
      });
      await act(async () => { renderer.root.findAllByProps({ role: 'tab' })[1].props.onClick(); await flush(); });
      const text = JSON.stringify(renderer.toJSON());
      expect(text).toContain(message);
      expect(text).not.toContain('registry-only-detail');
      await act(async () => renderer.unmount());
    }

    for (const diagnosticCode of [undefined, 'unexpected-code']) {
      const renderer = await renderPanel({ profiles: { profiles: [unavailable(diagnosticCode)], registryError: null } });
      await act(async () => { renderer.root.findAllByProps({ role: 'tab' })[1].props.onClick(); await flush(); });
      expect(JSON.stringify(renderer.toJSON())).toContain('Custom Codex quota unavailable');
      await act(async () => renderer.unmount());
    }
  });

  it('keeps default and valid custom profiles usable when another profile has a diagnostic', async () => {
    const invalid: CodexProfileQuota = {
      ...profile('Broken'), status: 'error', primary: undefined, secondary: undefined, diagnosticCode: 'invalid_row',
    };
    const renderer = await renderPanel({ profiles: { profiles: [invalid, profile('Working', 31)], registryError: null } });
    const tabs = renderer.root.findAllByProps({ role: 'tab' });
    expect(tabs.map((tab) => tab.children.join(''))).toEqual(['Default', 'Broken', 'Working']);
    await act(async () => { tabs[2].props.onClick(); await flush(); });
    expect(JSON.stringify(renderer.toJSON())).toContain('ChatGPT Plus');
    await act(async () => { renderer.root.findAllByProps({ role: 'tab' })[1].props.onClick(); await flush(); });
    expect(JSON.stringify(renderer.toJSON())).toContain('This profile entry is invalid.');
    await act(async () => renderer.unmount());
  });

  it('publishes healthy custom quota when Default account info fails, then replaces it on a later refresh', async () => {
    mockDefaultCalls();
    vi.mocked(backend.getCodexInfo)
      .mockRejectedValueOnce(new Error('default refresh failed'))
      .mockResolvedValueOnce({ connected: true, planType: 'plus' });
    vi.spyOn(backend, 'getCodexProfiles')
      .mockResolvedValueOnce({ profiles: [profile('Failed')], registryError: null })
      .mockResolvedValueOnce({ profiles: [profile('Later')], registryError: null });
    const onTrayQuotaSnapshotsChange = vi.fn();
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        manualRefreshNonce: 1,
        showCostSummary: false,
        sections: hiddenSections,
        onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });

    const published = onTrayQuotaSnapshotsChange.mock.calls
      .map(([snapshots]) => snapshots as CodexTrayAccountSnapshot[])
      .filter((snapshots) => snapshots.length > 0);
    expect(published).toEqual([
      [
        expect.objectContaining({ accountId: 'default', freshness: 'fresh' }),
        expect.objectContaining({ accountId: 'Failed', freshness: 'fresh' }),
      ],
      [
        expect.objectContaining({ accountId: 'default', freshness: 'fresh' }),
        expect.objectContaining({ accountId: 'Later', freshness: 'fresh' }),
      ],
    ]);
    await act(async () => renderer.unmount());
  });

  it('does not let delayed profiles from an older generation publish after a newer refresh', async () => {
    mockDefaultCalls();
    const older = deferred<CodexProfilesResponse>();
    vi.spyOn(backend, 'getCodexProfiles')
      .mockReturnValueOnce(older.promise)
      .mockResolvedValueOnce({ profiles: [profile('Newer')], registryError: null });
    const onTrayQuotaSnapshotsChange = vi.fn();
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        manualRefreshNonce: 1,
        showCostSummary: false,
        sections: hiddenSections,
        onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    await act(async () => {
      older.resolve({ profiles: [profile('Older')], registryError: null });
      await flush();
    });

    expect(onTrayQuotaSnapshotsChange).toHaveBeenCalledTimes(1);
    expect(onTrayQuotaSnapshotsChange).toHaveBeenCalledWith([
      expect.objectContaining({ accountId: 'default' }),
      expect.objectContaining({ accountId: 'Newer' }),
    ]);
    await act(async () => renderer.unmount());
  });

  it('keeps completed healthy custom generations while superseded work stays bounded', async () => {
    mockDefaultCalls();
    vi.mocked(backend.getCodexInfo)
      .mockRejectedValueOnce(new Error('failed 1'))
      .mockRejectedValueOnce(new Error('failed 2'))
      .mockRejectedValueOnce(new Error('failed 3'))
      .mockResolvedValueOnce({ connected: true, planType: 'plus' });
    vi.spyOn(backend, 'getCodexProfiles')
      .mockResolvedValueOnce({ profiles: [profile('Failed 1')], registryError: null })
      .mockResolvedValueOnce({ profiles: [profile('Failed 2')], registryError: null })
      .mockResolvedValueOnce({ profiles: [profile('Failed 3')], registryError: null })
      .mockResolvedValueOnce({ profiles: [profile('Current')], registryError: null });
    const onTrayQuotaSnapshotsChange = vi.fn();
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    for (const manualRefreshNonce of [1, 2, 3]) {
      await act(async () => {
        renderer.update(createElement(CodexPanel, {
          autoRefreshIntervalMs: 0,
          manualRefreshNonce,
          showCostSummary: false,
          sections: hiddenSections,
          onTrayQuotaSnapshotsChange,
        }));
        await flush();
      });
    }

    const published = onTrayQuotaSnapshotsChange.mock.calls
      .map(([snapshots]) => snapshots as CodexTrayAccountSnapshot[])
      .filter((snapshots) => snapshots.length > 0);
    expect(published).toHaveLength(4);
    expect(published.map((snapshots) => snapshots[1]?.accountId)).toEqual([
      'Failed 1', 'Failed 2', 'Failed 3', 'Current',
    ]);
    expect(published.every((snapshots) => snapshots.every((snapshot) => snapshot.freshness === 'fresh'))).toBe(true);
    await act(async () => renderer.unmount());
  });

  it('does not publish pending tray coordination after unmount', async () => {
    mockDefaultCalls();
    const profiles = deferred<CodexProfilesResponse>();
    vi.spyOn(backend, 'getCodexProfiles').mockReturnValue(profiles.promise);
    const onTrayQuotaSnapshotsChange = vi.fn();
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        onTrayQuotaSnapshotsChange,
      }));
      await flush();
    });
    await act(async () => renderer.unmount());
    await act(async () => {
      profiles.resolve({ profiles: [profile('Late')], registryError: null });
      await flush();
    });
    expect(onTrayQuotaSnapshotsChange).not.toHaveBeenCalled();
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

  it('does not mount default cost IPC when the default account is offline and no custom profiles exist', async () => {
    vi.spyOn(backend, 'getCostOverview').mockResolvedValue(emptyCostOverview());
    vi.spyOn(backend, 'getCostDaily').mockResolvedValue(emptyCostDaily());
    mockDefaultCalls();
    vi.mocked(backend.getCodexInfo).mockResolvedValue({ connected: false });
    vi.mocked(backend.getCodexRateLimits).mockResolvedValue({ connected: false });
    vi.mocked(backend.getCodexResetCredits).mockResolvedValue({
      connected: false,
      availableCount: 0,
      credits: [],
    });
    vi.spyOn(backend, 'getCodexProfiles').mockResolvedValue({ profiles: [], registryError: null });
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: true,
        sections: { ...hiddenSections, cost: true },
      }));
      await flush();
    });

    const text = JSON.stringify(renderer.toJSON());
    expect(text).toContain('Codex not connected');
    expect(text).not.toContain('API-equivalent usage');
    expect(backend.getCostOverview).not.toHaveBeenCalled();
    expect(backend.getCostDaily).not.toHaveBeenCalled();
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
