import { createElement } from 'react';
import { act, create, type ReactTestRenderer } from 'react-test-renderer';
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest';
import CostSummarySection, {
  ALL_COST_DISPLAY_NAME,
  costFooterLabels,
  formatRangeMoney,
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

  it('renders an unreadable range as n/a without lower-bound warning and labels it', async () => {
    vi.mocked(backend.getCostOverview).mockResolvedValue(overview({
      ranges: [range({ cost: null, costUsd: null, costIsLowerBound: false, sourceReadFailed: true })],
    }));
    renderer = await renderCost();
    const value = renderer.root.findByProps({ className: 'cost-range-value' });
    expect(value.children).toEqual(['n/a']);
    expect(JSON.stringify(renderer.toJSON())).toContain('usage files unreadable');
    expect(JSON.stringify(renderer.toJSON())).not.toContain('≥');
  });

  it('does not show the unreadable footer label for normal ranges', () => {
    expect(costFooterLabels(range(), overview())).not.toContain('usage files unreadable');
  });

  it('renders unreadable daily points as gaps and counts them as n/a', async () => {
    vi.mocked(backend.getCostDaily).mockResolvedValue(daily([
      point({ cost: null, costUsd: null, sourceReadFailed: true }),
    ]));
    renderer = await renderCost();
    const gap = renderer.root.findByProps({ className: 'spark-bar-hit gap' });
    expect(JSON.stringify(renderer.toJSON())).toContain('1 day n/a');
    await act(async () => gap.props.onMouseEnter());
    const snapshot = JSON.stringify(renderer.toJSON());
    expect(snapshot).toContain('2026-10-12 · n/a');
  });

  it('treats an unreadable day as a gap even if a zero amount and no tokens arrive', async () => {
    vi.mocked(backend.getCostDaily).mockResolvedValue(daily([
      point({ cost: 0, costUsd: 0, totalTokens: 0, sourceReadFailed: true }),
    ]));
    renderer = await renderCost();
    const gap = renderer.root.findByProps({ className: 'spark-bar-hit gap' });
    expect(JSON.stringify(renderer.toJSON())).toContain('1 day n/a');
    await act(async () => gap.props.onMouseEnter());
    expect(JSON.stringify(renderer.toJSON())).toContain('2026-10-12 · n/a');
  });

  it('renders a combined known cost as a warning and retains its monthly budget when another source is unreadable', async () => {
    localStorage.setItem('claude-quota-monthly-budgets', JSON.stringify({ claude: 10, codex: 10 }));
    const pricedRanges = [
      range({ range: 'today', cost: 5, costUsd: 5 }),
      range({ range: 'month', label: 'This Month', cost: 5, costUsd: 5 }),
    ];
    const unreadableRanges = pricedRanges.map((item) => ({
      ...item,
      cost: null,
      costUsd: null,
      costIsLowerBound: false,
      sourceReadFailed: true,
    }));
    vi.mocked(backend.getCostOverview).mockImplementation(async (source) => (
      source === 'codex'
        ? overview({ source: 'codex', displayName: 'Codex', ranges: unreadableRanges })
        : overview({ ranges: pricedRanges })
    ));
    await act(async () => {
      renderer = create(createElement(CostSummarySection, {
        source: ['claude', 'codex'] as const,
        autoRefreshIntervalMs: 0,
      }));
      await Promise.resolve();
      await Promise.resolve();
    });
    const snapshot = JSON.stringify(renderer.toJSON());
    expect(snapshot).toContain('≥ $5.00');
    expect(snapshot).toContain('usage files unreadable');
    expect(renderer.root.findAllByProps({ className: 'cost-range-value warning' })).toHaveLength(2);
    expect(renderer.root.findAllByProps({ className: 'budget-panel' })).toHaveLength(1);
  });

  it('renders a combined all-unreadable cost as n/a without a monthly budget', async () => {
    localStorage.setItem('claude-quota-monthly-budgets', JSON.stringify({ claude: 10, codex: 10 }));
    const unreadableRanges = [
      range({ range: 'today', cost: null, costUsd: null, sourceReadFailed: true }),
      range({ range: 'month', label: 'This Month', cost: null, costUsd: null, sourceReadFailed: true }),
    ];
    vi.mocked(backend.getCostOverview).mockImplementation(async (source) => overview({
      source,
      displayName: source === 'codex' ? 'Codex' : 'Claude',
      ranges: unreadableRanges,
    }));
    await act(async () => {
      renderer = create(createElement(CostSummarySection, {
        source: ['claude', 'codex'] as const,
        autoRefreshIntervalMs: 0,
      }));
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(JSON.stringify(renderer.toJSON())).toContain('n/a');
    expect(renderer.root.findAllByProps({ className: 'budget-panel' })).toHaveLength(0);
  });

  it('renders combined daily partial reads as a lower-bound bar and all failures as a gap', async () => {
    vi.mocked(backend.getCostDaily).mockImplementation(async (source) => (
      source === 'codex'
        ? { ...daily([point({ cost: null, costUsd: null, sourceReadFailed: true })]), source: 'codex' }
        : daily([point({ cost: 2, costUsd: 2 })])
    ));
    await act(async () => {
      renderer = create(createElement(CostSummarySection, {
        source: ['claude', 'codex'] as const,
        autoRefreshIntervalMs: 0,
      }));
      await Promise.resolve();
      await Promise.resolve();
    });
    const knownBar = renderer.root.findByProps({ className: 'spark-bar-hit' });
    await act(async () => knownBar.props.onMouseEnter());
    expect(JSON.stringify(renderer.toJSON())).toContain('2026-10-12 · ≥ $2.00');
    await act(async () => renderer?.unmount());
    renderer = undefined;

    vi.mocked(backend.getCostDaily).mockImplementation(async (source) => ({
      ...daily([point({ cost: null, costUsd: null, sourceReadFailed: true })]),
      source,
    }));
    await act(async () => {
      renderer = create(createElement(CostSummarySection, {
        source: ['claude', 'codex'] as const,
        autoRefreshIntervalMs: 0,
      }));
      await Promise.resolve();
      await Promise.resolve();
    });
    const gap = renderer.root.findByProps({ className: 'spark-bar-hit gap' });
    await act(async () => gap.props.onMouseEnter());
    expect(JSON.stringify(renderer.toJSON())).toContain('2026-10-12 · n/a');
  });

  it('shows n/a for seven unreadable days and a lower-bound partial total', async () => {
    const unreadableDays = Array.from({ length: 7 }, (_, index) => point({
      date: `2026-10-${String(index + 1).padStart(2, '0')}`,
      cost: null,
      costUsd: null,
      sourceReadFailed: true,
    }));
    vi.mocked(backend.getCostDaily).mockResolvedValue(daily(unreadableDays));
    renderer = await renderCost();
    let snapshot = JSON.stringify(renderer.toJSON());
    expect(snapshot).toContain('Past 7 days · n/a · 7 days n/a');
    expect(snapshot).not.toContain('Past 7 days · $0.00');
    await act(async () => renderer?.unmount());
    renderer = undefined;

    vi.mocked(backend.getCostDaily).mockResolvedValue(daily([
      point({ date: '2026-10-10', cost: 2, costUsd: 2 }),
      point({ date: '2026-10-11', cost: null, costUsd: null, sourceReadFailed: true }),
    ]));
    renderer = await renderCost();
    snapshot = JSON.stringify(renderer.toJSON());
    expect(snapshot).toContain('Past 7 days · ≥ $2.00 · 1 day n/a');
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

  it('marks a combined range with one unreadable source as a lower bound and all failures as n/a', () => {
    const partial = mergeCostOverviews([
      overview({ ranges: [range({ cost: 5, costUsd: 5 })] }),
      overview({ source: 'codex', ranges: [range({ cost: null, costUsd: null, sourceReadFailed: true })] }),
    ]);
    expect(partial.ranges[0]).toMatchObject({ cost: 5, sourceReadFailed: true, costIsLowerBound: true });
    expect(formatRangeMoney(partial.ranges[0].cost, partial.ranges[0].currency, partial.ranges[0].costIsLowerBound)).toBe('≥ $5.00');
    expect(costFooterLabels(partial.ranges[0], partial)).toContain('usage files unreadable');

    const allUnreadable = mergeCostOverviews([
      overview({ ranges: [range({ cost: null, costUsd: null, sourceReadFailed: true })] }),
      overview({ source: 'codex', ranges: [range({ cost: null, costUsd: null, sourceReadFailed: true })] }),
    ]);
    expect(allUnreadable.ranges[0]).toMatchObject({ cost: null, sourceReadFailed: true, costIsLowerBound: false });
    expect(formatRangeMoney(allUnreadable.ranges[0].cost, allUnreadable.ranges[0].currency)).toBe('n/a');
  });

  it('uses only the current overview range parse-error count when merging', () => {
    const merged = mergeCostOverviews([
      overview({ ranges: [range({ parseErrorEntries: 2 })] }),
      overview({ source: 'codex', ranges: [range({ parseErrorEntries: 0 })] }),
    ]);
    expect(merged.ranges[0].parseErrorEntries).toBe(2);
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

  it('keeps a lower bound when the unreadable source has no tokens, in either merge order', async () => {
    const failed = point({ cost: null, costUsd: null, totalTokens: 0, sourceReadFailed: true });
    const known = point({ cost: 2, costUsd: 2, totalTokens: 10 });
    for (const order of [[known, failed], [failed, known]]) {
      const merged = mergeDailySeries(order.map((day) => daily([day])));
      expect(merged[0]).toMatchObject({ cost: 2, costIsLowerBound: true, sourceReadFailed: true });
    }
    vi.mocked(backend.getCostDaily).mockImplementation(async (source) => (
      source === 'codex' ? { ...daily([failed]), source: 'codex' } : daily([known])
    ));
    await act(async () => {
      renderer = create(createElement(CostSummarySection, { source: ['claude', 'codex'] as const, autoRefreshIntervalMs: 0 }));
      await Promise.resolve();
      await Promise.resolve();
    });
    const bar = renderer.root.findByProps({ className: 'spark-bar-hit' });
    await act(async () => bar.props.onMouseEnter());
    expect(JSON.stringify(renderer.toJSON())).toContain('2026-10-12 · ≥ $2.00');
  });

  it('ignores a zero amount carried by an unreadable source when merging days', () => {
    const merged = mergeDailySeries([
      daily([point({ cost: 2, costUsd: 2, totalTokens: 10 })]),
      daily([point({ cost: 0, costUsd: 0, totalTokens: 0, sourceReadFailed: true })]),
      daily([point({ cost: 3, costUsd: 3, totalTokens: 10 })]),
    ]);
    expect(merged[0]).toMatchObject({ cost: 5, costIsLowerBound: true });
    const failedOnly = mergeDailySeries([
      daily([point({ cost: 1, costUsd: 1, totalTokens: 10 })]),
      daily([point({ cost: 7, costUsd: 7, totalTokens: 0, sourceReadFailed: true })]),
    ]);
    expect(failedOnly[0]).toMatchObject({ cost: 1, costIsLowerBound: true });
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
