import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import CostSummarySection, {
  ALL_COST_DISPLAY_NAME,
  costFooterLabels,
  mergeCostKinds,
  mergeDailySeries,
  mergeCostOverviews,
} from '../src/components/CostSummarySection';
import OverviewPanel from '../src/components/OverviewPanel';
import { backend } from '../src/services/backend';
import type { CostDailyPoint, CostDailySeries, CostOverview, CostRangeSummary } from '../src/types/models';

function tokens(totalTokens = 0) {
  return {
    inputTokens: 0,
    outputTokens: 0,
    reasoningTokens: 0,
    cacheCreationTokens: 0,
    cacheReadTokens: 0,
    totalTokens,
  };
}

function range(overrides: Partial<CostRangeSummary> = {}): CostRangeSummary {
  return {
    range: 'today',
    label: 'Today',
    currency: 'USD',
    cost: 12.4,
    costUsd: 12.4,
    estimatedCost: null,
    estimatedCostUsd: null,
    costKind: 'real',
    costIsLowerBound: false,
    unpricedModels: 0,
    tokens: tokens(),
    models: [],
    validEntries: 1,
    skippedEntries: 99,
    parseErrorEntries: 0,
    elapsedMs: 1,
    ...overrides,
  };
}

function overview(overrides: Partial<CostOverview> = {}): CostOverview {
  return {
    source: 'claude',
    displayName: 'Claude',
    currency: 'USD',
    generatedAt: '2026-10-12T00:00:00Z',
    cached: false,
    stale: false,
    ranges: [range()],
    ...overrides,
  };
}

function point(overrides: Partial<CostDailyPoint> = {}): CostDailyPoint {
  return {
    date: '2026-10-12',
    cost: 12.4,
    costUsd: 12.4,
    totalTokens: 10,
    costIsLowerBound: false,
    ...overrides,
  };
}

function daily(days: CostDailyPoint[]): CostDailySeries {
  return {
    source: 'claude',
    currency: 'USD',
    generatedAt: '2026-10-12T00:00:00Z',
    cached: false,
    stale: false,
    days,
  };
}

async function renderCost(): Promise<ReactTestRenderer> {
  let renderer!: ReactTestRenderer;
  await act(async () => {
    renderer = create(createElement(CostSummarySection, { source: 'claude', autoRefreshIntervalMs: 0 }));
    await Promise.resolve();
    await Promise.resolve();
  });
  return renderer;
}

describe('COST-F1 cost presentation', () => {
  let renderer: ReactTestRenderer | undefined;

  beforeAll(() => {
    (globalThis as Record<string, unknown>).IS_REACT_ACT_ENVIRONMENT = true;
  });

  beforeEach(() => {
    const values = new Map<string, string>();
    (globalThis as Record<string, unknown>).localStorage = {
      clear: () => values.clear(),
      getItem: (key: string) => values.get(key) ?? null,
      removeItem: (key: string) => values.delete(key),
      setItem: (key: string, value: string) => values.set(key, value),
    };
    vi.spyOn(backend, 'getCostOverview').mockResolvedValue(overview());
    vi.spyOn(backend, 'getCostDaily').mockResolvedValue(daily([]));
  });

  afterEach(async () => {
    if (renderer) await act(async () => renderer?.unmount());
    renderer = undefined;
    vi.restoreAllMocks();
  });

  it('renders a lower-bound range with warning styling', async () => {
    vi.mocked(backend.getCostOverview).mockResolvedValue(overview({
      ranges: [range({ costIsLowerBound: true })],
    }));
    renderer = await renderCost();
    const value = renderer.root.findByProps({ className: 'cost-range-value warning' });
    expect(value.children).toEqual(['≥ $12.40']);
    expect(value.props.title).toContain('at least');
  });

  it('renders an unpriced day as a gap and shows n/a on hover', async () => {
    vi.mocked(backend.getCostDaily).mockResolvedValue(daily([
      point({ cost: null, costUsd: null, totalTokens: 10, costIsLowerBound: true }),
    ]));
    renderer = await renderCost();
    const gap = renderer.root.findByProps({ className: 'spark-bar-hit gap' });
    expect(gap.findAll((node) => (
      typeof node.props.className === 'string' && node.props.className.split(' ').includes('spark-bar')
    ))).toHaveLength(0);
    await act(async () => gap.props.onMouseEnter());
    expect(JSON.stringify(renderer.toJSON())).toContain('2026-10-12 · n/a');
  });

  it('shows the lower-bound marker when hovering a partially priced day', async () => {
    vi.mocked(backend.getCostDaily).mockResolvedValue(daily([
      point({ costIsLowerBound: true }),
    ]));
    renderer = await renderCost();
    const hit = renderer.root.findByProps({ className: 'spark-bar-hit' });
    await act(async () => hit.props.onMouseEnter());
    expect(JSON.stringify(renderer.toJSON())).toContain('2026-10-12 · ≥ $12.40');
  });

  it('marks 7D totals as lower bounds and counts one n/a day', async () => {
    vi.mocked(backend.getCostDaily).mockResolvedValue(daily([
      point({ date: '2026-10-10', costIsLowerBound: true }),
      point({ date: '2026-10-11', cost: null, costUsd: null, totalTokens: 10 }),
    ]));
    renderer = await renderCost();
    const snapshot = JSON.stringify(renderer.toJSON());
    expect(snapshot).toContain('Past 7 days · ≥ $12.40 · 1 day n/a');
  });

  it('marks 7D totals as lower bounds and counts three n/a days', async () => {
    vi.mocked(backend.getCostDaily).mockResolvedValue(daily([
      point({ date: '2026-10-09', costIsLowerBound: true }),
      point({ date: '2026-10-10', cost: null, costUsd: null, totalTokens: 10 }),
      point({ date: '2026-10-11', cost: null, costUsd: null, totalTokens: 10 }),
      point({ date: '2026-10-12', cost: null, costUsd: null, totalTokens: 10 }),
    ]));
    renderer = await renderCost();
    expect(JSON.stringify(renderer.toJSON())).toContain('Past 7 days · ≥ $12.40 · 3 days n/a');
  });

  it('shows footer labels without skipped entries', async () => {
    vi.mocked(backend.getCostOverview).mockResolvedValue(overview({
      cached: true,
      ranges: [range({
        unpricedModels: 2,
        parseErrorEntries: 1,
        costKind: 'mixed',
        skippedEntries: 500,
      })],
    }));
    renderer = await renderCost();
    const snapshot = JSON.stringify(renderer.toJSON());
    expect(snapshot).toContain('Today · 2 models unpriced · 1 parse error · Estimated · Cached');
    expect(snapshot).not.toContain('500');
  });

  it('shows Stale instead of Cached when a snapshot is stale', async () => {
    vi.mocked(backend.getCostOverview).mockResolvedValue(overview({ stale: true, cached: true }));
    renderer = await renderCost();
    const snapshot = JSON.stringify(renderer.toJSON());
    expect(snapshot).toContain('Stale');
    expect(snapshot).not.toContain('Cached');
  });

  it('shows Stale when only the daily series is a stale snapshot', async () => {
    vi.mocked(backend.getCostOverview).mockResolvedValue(overview({ cached: true, stale: false }));
    vi.mocked(backend.getCostDaily).mockResolvedValue({ ...daily([point()]), cached: true, stale: true });
    renderer = await renderCost();
    const snapshot = JSON.stringify(renderer.toJSON());
    expect(snapshot).toContain('Stale');
    expect(snapshot).not.toContain('Cached');
  });

  it('shows Stale on the combined card when one source has a stale daily series', async () => {
    vi.mocked(backend.getCostOverview).mockResolvedValue(overview());
    vi.mocked(backend.getCostDaily).mockImplementation(async (source) => (
      source === 'codex' ? { ...daily([point()]), source: 'codex', cached: true, stale: true } : daily([point()])
    ));
    await act(async () => {
      renderer = create(createElement(CostSummarySection, { source: ['claude', 'codex'] as const, autoRefreshIntervalMs: 0 }));
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(JSON.stringify(renderer.toJSON())).toContain('Stale');
  });

  it('merges lower-bound fields and uses the named combined card', () => {
    const merged = mergeCostOverviews([
      overview({ ranges: [range({ unpricedModels: 1, parseErrorEntries: 2, costIsLowerBound: true })] }),
      overview({ source: 'codex', stale: true, ranges: [range({ cost: null, costUsd: null, tokens: tokens(10) })] }),
    ]);
    expect(merged.displayName).toBe(ALL_COST_DISPLAY_NAME);
    expect(merged.stale).toBe(true);
    expect(merged.ranges[0]).toMatchObject({ costIsLowerBound: true, unpricedModels: 1, parseErrorEntries: 2 });
  });

  it('ignores sources without usage when merging cost kinds', () => {
    expect(mergeCostKinds(['real', 'none'])).toBe('real');
    expect(mergeCostKinds(['none', 'none'])).toBe('none');
    expect(mergeCostKinds(['estimated_proxy', 'none'])).toBe('estimated_proxy');
    expect(mergeCostKinds(['real', 'estimated_proxy'])).toBe('mixed');
    const merged = mergeCostOverviews([
      overview({ ranges: [range({ costKind: 'real' })] }),
      overview({ source: 'cursor', ranges: [range({ costKind: 'none', cost: null, costUsd: null })] }),
    ]);
    expect(merged.ranges[0].costKind).toBe('real');
  });

  it('marks a merged range as a lower bound when one source has usage but no price', () => {
    const merged = mergeCostOverviews([
      overview({ ranges: [range({ cost: 5, costUsd: 5, tokens: tokens(10) })] }),
      overview({ source: 'codex', ranges: [range({ cost: null, costUsd: null, tokens: tokens(10) })] }),
    ]);
    expect(merged.ranges[0].costIsLowerBound).toBe(true);
    const complete = mergeCostOverviews([
      overview({ ranges: [range({ cost: 5, costUsd: 5, tokens: tokens(10) })] }),
      overview({ source: 'codex', ranges: [range({ cost: null, costUsd: null, tokens: tokens(0) })] }),
    ]);
    expect(complete.ranges[0].costIsLowerBound).toBe(false);
  });

  it('labels estimated-only pricing as Estimated', () => {
    expect(costFooterLabels(range({ costKind: 'estimated_proxy' }), overview())).toContain('Estimated');
    expect(costFooterLabels(range({ costKind: 'real' }), overview())).not.toContain('Estimated');
  });

  it('marks a merged day as a lower bound when one source has usage but no price', () => {
    const merged = mergeDailySeries([
      daily([point({ cost: 2, costUsd: 2, totalTokens: 10 })]),
      daily([point({ cost: null, costUsd: null, totalTokens: 5 })]),
    ]);
    expect(merged[0]).toMatchObject({ cost: 2, costIsLowerBound: true });
  });

  it('merges stale state for daily series', () => {
    const merged = mergeDailySeries([
      daily([point({ date: '2026-10-11' })]),
      { ...daily([point({ date: '2026-10-12' })]), stale: true },
    ]);
    expect(merged.stale).toBe(true);
  });

  it('passes the combined card name to OverviewPanel', async () => {
    let panel!: ReactTestRenderer;
    await act(async () => {
      panel = create(createElement(OverviewPanel, {
        summaries: [],
        mostConstrained: [],
        upcomingResets: [],
        costRefreshKey: 0,
        onProviderSelect: () => undefined,
      }));
      await Promise.resolve();
    });
    expect(JSON.stringify(panel.toJSON())).toContain(ALL_COST_DISPLAY_NAME);
    await act(async () => panel.unmount());
  });

  it('keeps fully priced range text and classes unchanged', async () => {
    renderer = await renderCost();
    const value = renderer.root.findByProps({ className: 'cost-range-value' });
    expect(value.children).toEqual(['$12.40']);
  });
});
