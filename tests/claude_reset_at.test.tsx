import { createElement } from 'react';
import { act, create, type ReactTestInstance, type ReactTestRenderer } from 'react-test-renderer';
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import ClaudePanel from '../src/components/ClaudePanel';
import CodexPanel from '../src/components/CodexPanel';
import { backend } from '../src/services/backend';
import { formatResetAt } from '../src/utils/quota_format';

const NOW = new Date('2026-09-27T12:00:00Z');
const FUTURE = new Date('2026-09-28T00:15:00.123Z');
const hiddenSections = { timeline: false, cost: false, trend: false, tips: false };

function expectedResetAt(value: number): string {
  const date = new Date(value * 1000);
  const time = date.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit' });
  if (date.toDateString() === NOW.toDateString()) return `Today, ${time}`;
  const day = date.toLocaleDateString('en-US', { weekday: 'short', month: 'short', day: 'numeric' });
  return `${day}, ${time}`;
}

function cardForLabel(renderer: ReactTestRenderer, label: string): ReactTestInstance {
  const card = renderer.root.findAll((node) => (
    typeof node.props.className === 'string'
      && node.props.className.split(' ').includes('quota-card')
      && node.findAllByProps({ className: 'quota-label' }).some((title) => title.children.join('') === label)
  ))[0];
  if (!card) throw new Error(`Missing quota card: ${label}`);
  return card;
}

function renderedText(renderer: ReactTestRenderer): string {
  return JSON.stringify(renderer.toJSON());
}

beforeAll(() => {
  (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
});

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

afterAll(() => {
  delete (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT;
});

describe('Claude reset-at display', () => {
  it('formats numeric and strict RFC3339 reset times identically', () => {
    const epochSeconds = FUTURE.getTime() / 1000;
    const expected = expectedResetAt(epochSeconds);

    expect(formatResetAt(epochSeconds)).toBe(expected);
    expect(formatResetAt('2026-09-28T00:15:00.123Z')).toBe(expected);
    expect(formatResetAt('2026-09-28T09:15:00.123+09:00')).toBe(expected);
    expect(formatResetAt('2026-09-28T00:15:00.123456+00:00')).toBe(expected);
    expect(formatResetAt('')).toBe('');
    expect(formatResetAt('garbage')).toBe('');
    expect(formatResetAt('2026-02-30T00:00:00Z')).toBe('');
    expect(formatResetAt('2026-09-28T25:00:00Z')).toBe('');
    expect(formatResetAt(undefined)).toBe('');
    expect(formatResetAt(0)).toBe('');
  });

  it('shows a future weekly reset as a semantic time while missing session reset stays N/A', async () => {
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(ClaudePanel, {
        quota: {
          connected: true,
          session: { used: 5, limit: 100, percentage: 5 },
          weeklyTotal: { used: 40, limit: 100, percentage: 40, resetTime: FUTURE.toISOString() },
        },
        loading: false,
        error: null,
        windowVisible: false,
        costRefreshKey: 0,
        onRetry: vi.fn(),
        sections: hiddenSections,
      }));
    });

    const weeklyTime = cardForLabel(renderer, 'All models').findByType('time');
    expect(weeklyTime.props.dateTime).toBe(FUTURE.toISOString());
    expect(weeklyTime.children.join('')).toBe(expectedResetAt(FUTURE.getTime() / 1000));
    const session = cardForLabel(renderer, '5-hour window');
    expect(session.findByProps({ className: 'reset-text' }).children.join('')).toBe('Resets in N/A');
    expect(session.findAllByType('time')).toHaveLength(0);
    await act(async () => renderer.unmount());
  });

  it('hides the absolute reset time once the reset is past', async () => {
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(ClaudePanel, {
        quota: {
          connected: true,
          weeklyTotal: { used: 40, limit: 100, percentage: 40, resetTime: '2026-09-27T11:59:59Z' },
        },
        loading: false,
        error: null,
        windowVisible: false,
        costRefreshKey: 0,
        onRetry: vi.fn(),
        sections: hiddenSections,
      }));
    });

    const card = cardForLabel(renderer, 'All models');
    expect(card.findByProps({ className: 'reset-text' }).children.join('')).toBe('Resets in Soon');
    expect(card.findAllByType('time')).toHaveLength(0);
    await act(async () => renderer.unmount());
  });

  it('keeps all six Claude reset times on their own cards', async () => {
    const labels = ['5-hour window', 'All models', 'Opus', 'Sonnet', 'Claude Design', 'Fable 5'];
    const resets = labels.map((_, index) => new Date(FUTURE.getTime() + index * 60 * 60 * 1000).toISOString());
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(ClaudePanel, {
        quota: {
          connected: true,
          session: { used: 1, limit: 100, percentage: 1, resetTime: resets[0] },
          weeklyTotal: { used: 2, limit: 100, percentage: 2, resetTime: resets[1] },
          weeklyOpus: { used: 3, limit: 100, percentage: 3, resetTime: resets[2] },
          weeklySonnet: { used: 4, limit: 100, percentage: 4, resetTime: resets[3] },
          weeklyDesign: { used: 5, limit: 100, percentage: 5, resetTime: resets[4] },
          weeklyFable5: { used: 6, limit: 100, percentage: 6, resetTime: resets[5] },
        },
        loading: false,
        error: null,
        windowVisible: false,
        costRefreshKey: 0,
        onRetry: vi.fn(),
        sections: hiddenSections,
      }));
    });

    labels.forEach((label, index) => {
      expect(cardForLabel(renderer, label).findByType('time').props.dateTime).toBe(resets[index]);
    });
    await act(async () => renderer.unmount());
  });

  it('keeps Codex reset-at formatting unchanged', async () => {
    const resetEpoch = FUTURE.getTime() / 1000;
    vi.spyOn(backend, 'getCodexInfo').mockResolvedValue({ connected: true, planType: 'plus' });
    vi.spyOn(backend, 'getCodexRateLimits').mockResolvedValue({
      connected: true,
      planType: 'plus',
      primary: { usedPercent: 40, windowMinutes: 300, resetsAt: resetEpoch },
    });
    vi.spyOn(backend, 'getCodexResetCredits').mockResolvedValue({ connected: true, availableCount: 0, credits: [] });
    vi.spyOn(backend, 'getCodexWeeklyQuota').mockResolvedValue({});
    let renderer!: ReactTestRenderer;
    await act(async () => {
      renderer = create(createElement(CodexPanel, {
        autoRefreshIntervalMs: 0,
        showCostSummary: false,
        sections: hiddenSections,
      }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(renderedText(renderer)).toContain(expectedResetAt(resetEpoch));
    await act(async () => renderer.unmount());
  });
});
