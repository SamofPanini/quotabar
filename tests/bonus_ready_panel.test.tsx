import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from 'vitest';
import CodexPanel from '../src/components/CodexPanel';
import { backend } from '../src/services/backend';
import type { CodexRateLimits, CodexResetCredits } from '../src/types/models';

const hiddenSections = { timeline: false, cost: false, trend: false, tips: false };

const blockedLimits = {
  connected: true,
  planType: 'plus',
  ordinaryUsageAllowed: false,
  secondary: {
    usedPercent: 100,
    windowMinutes: 10_080,
    resetsAt: 1_787_961_600,
  },
};

const emptyLimits: CodexRateLimits = {
  connected: false,
  error: 'Network error',
};

const leftoverCredits: CodexResetCredits = {
  connected: true,
  availableCount: 1,
  credits: [{ status: 'available', expiresAt: '2026-09-10T00:00:00Z' }],
};

const disconnectedCredits: CodexResetCredits = {
  connected: false,
  availableCount: 0,
  credits: [],
  error: 'Network error',
};

beforeAll(() => {
  (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
});

afterEach(() => {
  vi.restoreAllMocks();
});

afterAll(() => {
  delete (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT;
});

async function render_panel(
  credits: CodexResetCredits,
  onBonusReadyChange: (ready: { exhausted: boolean; availableCount: number }) => void,
  manualRefreshNonce = 0,
  limits: CodexRateLimits = blockedLimits,
): Promise<ReactTestRenderer> {
  vi.spyOn(backend, 'getCodexInfo').mockResolvedValue({ connected: true, planType: 'plus' });
  vi.spyOn(backend, 'getCodexRateLimits').mockResolvedValue(limits);
  vi.spyOn(backend, 'getCodexResetCredits').mockResolvedValue(credits);
  vi.spyOn(backend, 'getCodexWeeklyQuota').mockResolvedValue({});
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(createElement(CodexPanel, {
      autoRefreshIntervalMs: 0,
      showCostSummary: false,
      sections: hiddenSections,
      manualRefreshNonce,
      onBonusReadyChange,
    }));
    await Promise.resolve();
    await Promise.resolve();
  });
  return renderer;
}

describe('Codex bonusReady reporting', () => {
  it('does not report a snapshot while reset credits are disconnected', async () => {
    const onBonusReadyChange = vi.fn();
    const renderer = await render_panel(disconnectedCredits, onBonusReadyChange);
    expect(onBonusReadyChange).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('does not emit bonus-ready when credits recover while ordinary usage is blocked', async () => {
    const onBonusReadyChange = vi.fn();
    const renderer = await render_panel(disconnectedCredits, onBonusReadyChange);
    expect(onBonusReadyChange).not.toHaveBeenCalled();

    vi.spyOn(backend, 'getCodexResetCredits').mockResolvedValue(leftoverCredits);
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        manualRefreshNonce: 1,
        onBonusReadyChange,
      }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(onBonusReadyChange).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('does not emit bonus-ready for blocked ordinary usage with available credits', async () => {
    const onBonusReadyChange = vi.fn();
    const renderer = await render_panel(leftoverCredits, onBonusReadyChange);
    expect(onBonusReadyChange).not.toHaveBeenCalled();

    vi.spyOn(backend, 'getCodexResetCredits').mockResolvedValue(disconnectedCredits);
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        manualRefreshNonce: 1,
        onBonusReadyChange,
      }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(onBonusReadyChange).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('does not report while the official weekly window is missing', async () => {
    const onBonusReadyChange = vi.fn();
    const renderer = await render_panel(leftoverCredits, onBonusReadyChange, 0, emptyLimits);
    expect(onBonusReadyChange).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('does not emit bonus-ready when a blocked numeric weekly window appears', async () => {
    const onBonusReadyChange = vi.fn();
    const renderer = await render_panel(leftoverCredits, onBonusReadyChange, 0, emptyLimits);
    expect(onBonusReadyChange).not.toHaveBeenCalled();

    vi.spyOn(backend, 'getCodexRateLimits').mockResolvedValue(blockedLimits);
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        manualRefreshNonce: 1,
        onBonusReadyChange,
      }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(onBonusReadyChange).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('does not emit bonus-ready when a blocked weekly window later disappears', async () => {
    const onBonusReadyChange = vi.fn();
    const renderer = await render_panel(leftoverCredits, onBonusReadyChange);
    expect(onBonusReadyChange).not.toHaveBeenCalled();

    vi.spyOn(backend, 'getCodexRateLimits').mockResolvedValue(emptyLimits);
    await act(async () => {
      renderer.update(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
        manualRefreshNonce: 1,
        onBonusReadyChange,
      }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(onBonusReadyChange).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });

  it('does not emit bonus-ready when ordinary usage availability is unknown', async () => {
    const onBonusReadyChange = vi.fn();
    const renderer = await render_panel(leftoverCredits, onBonusReadyChange, 0, {
      ...blockedLimits,
      ordinaryUsageAllowed: undefined,
    });
    expect(onBonusReadyChange).not.toHaveBeenCalled();
    await act(async () => renderer.unmount());
  });
});
